//! `Ctrl+Z`: the toggle between a person's root console (or the console) and
//! a context the person performs under it. The decision is
//! `kaijutsu_client::shell_toggle`, shared with `kaijutsu-tui`
//! (`docs/character.md`, "A person, the console, MCP sessions, the judge").
//!
//! The systems below run it on the kernel in an async task, make the `create`
//! call when the decision says so, and move the screen through the ordinary
//! `ContextSwitchRequested` path.

use bevy::prelude::*;
use kaijutsu_client::{ShellToggle, shell_toggle};
use kaijutsu_types::ContextId;

use crate::cell::{ContextSwitchRequested, DocumentCache};
use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};
use crate::input::{Action, ActionFired};
use crate::view::components::GlobalErrorQueue;

/// True while a toggle's listing and create are on the kernel. A second press
/// in that window is refused, so it cannot create a second shell child.
#[derive(Resource, Default)]
pub struct ShellToggleInFlight(pub bool);

/// Consume `Action::ToggleShellContext`: ask the kernel for the toggle target
/// in a task, and let [`handle_shell_toggle_result`] move the screen. The
/// listing is read on the kernel at press time, as the tui's does, so a shell
/// child made a moment ago is found and not created twice.
pub fn handle_toggle_shell_context(
    mut actions: MessageReader<ActionFired>,
    doc_cache: Res<DocumentCache>,
    actor: Option<Res<RpcActor>>,
    results: Res<RpcResultChannel>,
    mut in_flight: ResMut<ShellToggleInFlight>,
    mut errors: ResMut<GlobalErrorQueue>,
    time: Res<Time>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::ToggleShellContext) {
            continue;
        }
        if in_flight.0 {
            errors.push("ctrl+z", "a context toggle is already running", time.elapsed_secs_f64());
            continue;
        }
        let Some(current) = doc_cache.active_id() else {
            errors.push("ctrl+z", "no context attached", time.elapsed_secs_f64());
            continue;
        };
        let Some(actor) = actor.as_ref() else {
            errors.push("ctrl+z", "not connected to a kernel", time.elapsed_secs_f64());
            continue;
        };
        in_flight.0 = true;
        let handle = actor.handle.clone();
        let tx = results.sender();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                let result = toggle_on_kernel(&handle, current).await;
                let _ = tx.send(RpcResultMessage::ShellToggled { result });
            })
            .detach();
    }
}

/// The kernel side of one press: list, decide, and create when the decision
/// says so. The create matches the tui's `create_child`: the client's own
/// working directory, refused by the kernel, falls back to the parent's.
async fn toggle_on_kernel(
    handle: &kaijutsu_client::ActorHandle,
    current: ContextId,
) -> Result<ContextId, String> {
    let contexts = handle
        .list_contexts()
        .await
        .map_err(|e| format!("cannot list contexts: {e}"))?;
    let on_screen = contexts
        .iter()
        .find(|c| c.id == current)
        .ok_or_else(|| "the context on screen is not in the kernel's listing".to_string())?;
    let me = handle
        .whoami()
        .await
        .map_err(|e| format!("cannot read who this seat is: {e}"))?
        .username;
    match shell_toggle(on_screen, &contexts, &me) {
        ShellToggle::Switch(id) => Ok(id),
        ShellToggle::Refuse(why) => Err(why),
        ShellToggle::Create { parent, label } => {
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|dir| dir.into_os_string().into_string().ok());
            // `kj context create <person>`: the person at this seat performs
            // it, and it answers to the context it was opened from. A
            // person's context runs no model.
            let created = handle
                .create_context_in_client_cwd(parent, &label, &me, cwd.as_deref())
                .await
                .map_err(|e| format!("cannot create {label}: {e}"))?;
            if let Some(refused) = created.cwd_refused {
                warn!("the shell took the parent's cwd; the kernel refused this one: {refused}");
            }
            Ok(created.id)
        }
    }
}

/// Drain [`RpcResultMessage::ShellToggled`]: switch to the target, or report
/// the refusal or failure in the error queue. Either way the next press may
/// run.
pub fn handle_shell_toggle_result(
    mut results: MessageReader<RpcResultMessage>,
    mut in_flight: ResMut<ShellToggleInFlight>,
    mut switch: MessageWriter<ContextSwitchRequested>,
    mut errors: ResMut<GlobalErrorQueue>,
    time: Res<Time>,
) {
    for msg in results.read() {
        let RpcResultMessage::ShellToggled { result } = msg else {
            continue;
        };
        in_flight.0 = false;
        match result {
            Ok(context_id) => {
                switch.write(ContextSwitchRequested { context_id: *context_id });
            }
            Err(why) => errors.push("ctrl+z", why.clone(), time.elapsed_secs_f64()),
        }
    }
}

#[cfg(test)]
mod system_tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<ActionFired>()
            .add_message::<RpcResultMessage>()
            .add_message::<ContextSwitchRequested>()
            .init_resource::<GlobalErrorQueue>()
            .init_resource::<DocumentCache>()
            .init_resource::<ShellToggleInFlight>()
            .insert_resource(RpcResultChannel::new())
            .add_systems(Update, (handle_toggle_shell_context, handle_shell_toggle_result).chain());
        app
    }

    fn errors(app: &App) -> Vec<String> {
        app.world().resource::<GlobalErrorQueue>().entries.iter().map(|e| e.message.clone()).collect()
    }

    fn switches(app: &App) -> Vec<ContextId> {
        let switches = app.world().resource::<Messages<ContextSwitchRequested>>();
        switches.iter_current_update_messages().map(|s| s.context_id).collect()
    }

    fn press(app: &mut App) {
        app.world_mut()
            .write_message(ActionFired::new(Action::ToggleShellContext, crate::input::InputContext::Global));
        app.update();
    }

    #[test]
    fn a_resolved_target_moves_the_screen_and_clears_the_guard() {
        let mut app = app();
        let target = ContextId::new();
        app.world_mut().resource_mut::<ShellToggleInFlight>().0 = true;
        app.world_mut()
            .write_message(RpcResultMessage::ShellToggled { result: Ok(target) });
        app.update();
        assert_eq!(switches(&app), vec![target]);
        assert!(!app.world().resource::<ShellToggleInFlight>().0);
    }

    /// A refusal or a failed create moves nothing and says why.
    #[test]
    fn a_refused_toggle_moves_nothing_and_says_why() {
        let mut app = app();
        app.world_mut().resource_mut::<ShellToggleInFlight>().0 = true;
        app.world_mut().write_message(RpcResultMessage::ShellToggled {
            result: Err("Ctrl+Z toggles a root and its shell; this context is neither".into()),
        });
        app.update();
        assert!(switches(&app).is_empty());
        assert!(errors(&app).iter().any(|e| e.contains("this context is neither")), "{:?}", errors(&app));
        assert!(!app.world().resource::<ShellToggleInFlight>().0);
    }

    /// The guard: a second press before the first toggle lands is refused, so
    /// two presses cannot create two shell children.
    #[test]
    fn a_press_while_a_toggle_is_in_flight_is_refused() {
        let mut app = app();
        app.world_mut().resource_mut::<DocumentCache>().set_active(ContextId::new());
        app.world_mut().resource_mut::<ShellToggleInFlight>().0 = true;
        press(&mut app);
        assert!(errors(&app).iter().any(|e| e.contains("already running")), "{:?}", errors(&app));
        assert!(switches(&app).is_empty());
    }

    #[test]
    fn a_press_with_no_context_on_screen_says_so() {
        let mut app = app();
        press(&mut app);
        assert!(errors(&app).iter().any(|e| e.contains("no context attached")), "{:?}", errors(&app));
    }
}
