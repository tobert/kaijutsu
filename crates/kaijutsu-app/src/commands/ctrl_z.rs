//! `Ctrl+Z`: the toggle between a root context and its shell child. The app
//! twin of `kaijutsu-tui`'s `ctrl_z` (`docs/character.md`, "The root console
//! and its shell").
//!
//! [`toggle`] is pure: it turns the context on screen and the kernel's listing
//! into a [`Toggle`]. The systems below run it on the kernel in an async
//! task, make the `create` call when the decision says so, and move the screen
//! through the ordinary `ContextSwitchRequested` path.

use bevy::prelude::*;
use kaijutsu_client::ContextInfo;
use kaijutsu_types::{CONSOLE_CONTEXT_TYPE, ContextId, ROOT_CONTEXT_TYPE, SHELL_CONTEXT_TYPE};

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
    match toggle(on_screen, &contexts) {
        Toggle::Switch(id) => Ok(id),
        Toggle::Refuse(why) => Err(why.to_string()),
        Toggle::Create { parent, label } => {
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|dir| dir.into_os_string().into_string().ok());
            let performer = handle
                .performer_for_new_contexts(parent, None, "--character <name>")
                .await
                .map_err(|e| format!("cannot create {label}: {e}"))?;
            let created = handle
                .create_context_in_client_cwd(parent, &label, SHELL_CONTEXT_TYPE, &performer, cwd.as_deref())
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

/// What one `Ctrl+Z` press asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toggle {
    /// Switch the screen to this context: a root's live shell child, or a
    /// shell's parent.
    Switch(ContextId),
    /// A root with no live shell child: create one under `parent`, labelled
    /// `label`, and then switch to it.
    Create { parent: ContextId, label: String },
    /// The context on screen is neither a root nor a shell. Say so; do not
    /// move.
    Refuse(&'static str),
}

/// The label a root's shell child is created with: `<root-label>-shell`.
pub fn shell_label(root_label: &str) -> String {
    format!("{root_label}-shell")
}

/// Decide what `Ctrl+Z` does from the context on screen and the kernel's
/// listing.
///
/// From a root: its live shell child, else a create. A child under the same
/// root with the predictable label is preferred, so two shell children (one
/// made by hand) resolve the same way every time. From a shell: its parent.
/// From any other type: refuse.
pub fn toggle(current: &ContextInfo, contexts: &[ContextInfo]) -> Toggle {
    match current.context_type.as_str() {
        ROOT_CONTEXT_TYPE | CONSOLE_CONTEXT_TYPE => {
            let children: Vec<&ContextInfo> = contexts
                .iter()
                .filter(|c| {
                    c.context_type == SHELL_CONTEXT_TYPE
                        && c.forked_from == Some(current.id)
                        && is_live(c)
                })
                .collect();
            let want = shell_label(&current.label);
            match children.iter().find(|c| c.label == want).or(children.first()) {
                Some(child) => Toggle::Switch(child.id),
                None => Toggle::Create { parent: current.id, label: want },
            }
        }
        SHELL_CONTEXT_TYPE => match current.forked_from {
            Some(parent) => Toggle::Switch(parent),
            None => Toggle::Refuse("Ctrl+Z: this shell has no root to return to"),
        },
        _ => Toggle::Refuse("Ctrl+Z toggles a root and its shell; this context is neither"),
    }
}

/// Archived and concluded contexts are not reused: a live shell child is the
/// one `Ctrl+Z` returns to.
fn is_live(context: &ContextInfo) -> bool {
    !context.archived && context.concluded_at.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::Status;

    fn ctx(label: &str, context_type: &str, forked_from: Option<ContextId>) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from,
            provider: String::new(),
            model: String::new(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: context_type.to_string(),
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
            input_tokens: None,
            cost: None,
        }
    }

    fn root(label: &str) -> ContextInfo {
        ctx(label, ROOT_CONTEXT_TYPE, None)
    }

    fn shell_of(parent: &ContextInfo, label: &str) -> ContextInfo {
        ctx(label, SHELL_CONTEXT_TYPE, Some(parent.id))
    }

    #[test]
    fn a_root_with_no_shell_child_creates_one_with_the_predictable_label() {
        let amy = root("amy");
        let contexts = vec![amy.clone()];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    #[test]
    fn a_root_switches_to_its_live_shell_child() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), shell.clone()];
        assert_eq!(toggle(&amy, &contexts), Toggle::Switch(shell.id));
    }

    /// The reuse mutation: a root that ignores existing children would create
    /// a second shell every press.
    #[test]
    fn a_root_reuses_the_child_it_already_has_and_does_not_create_another() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), shell.clone()];
        assert!(!matches!(toggle(&amy, &contexts), Toggle::Create { .. }));
    }

    #[test]
    fn an_archived_or_concluded_shell_child_is_not_reused() {
        let amy = root("amy");
        let mut archived = shell_of(&amy, "amy-shell");
        archived.archived = true;
        let mut concluded = shell_of(&amy, "amy-shell-2");
        concluded.concluded_at = Some(2_000);
        let contexts = vec![amy.clone(), archived, concluded];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    /// A shell under another root is not this root's shell.
    #[test]
    fn a_shell_under_another_root_is_not_this_roots() {
        let amy = root("amy");
        let bob = root("bob");
        let theirs = shell_of(&bob, "bob-shell");
        let contexts = vec![amy.clone(), bob, theirs];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    /// Two shell children: the one with the predictable label wins, whatever
    /// the listing order.
    #[test]
    fn the_predictable_label_wins_among_shell_children() {
        let amy = root("amy");
        let other = shell_of(&amy, "scratch");
        let named = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), other, named.clone()];
        assert_eq!(toggle(&amy, &contexts), Toggle::Switch(named.id));
    }

    #[test]
    fn a_shell_switches_back_to_its_parent() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), shell.clone()];
        assert_eq!(toggle(&shell, &contexts), Toggle::Switch(amy.id));
    }

    #[test]
    fn a_shell_with_no_parent_refuses_rather_than_guessing() {
        let orphan = ctx("orphan", SHELL_CONTEXT_TYPE, None);
        assert!(matches!(toggle(&orphan, &[orphan.clone()]), Toggle::Refuse(_)));
    }

    /// Any other type does nothing: `Ctrl+Z` never creates under a coder.
    #[test]
    fn any_other_context_refuses_and_creates_nothing() {
        let lane = ctx("lane", "coder", None);
        let contexts = vec![lane.clone()];
        assert!(matches!(toggle(&lane, &contexts), Toggle::Refuse(_)));
    }

    #[test]
    fn shell_label_is_root_label_with_shell_suffix() {
        assert_eq!(shell_label("amy"), "amy-shell");
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
