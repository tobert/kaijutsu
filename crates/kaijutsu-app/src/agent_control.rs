//! The agent-control latch: whether an agent may fire actions over BRP.
//!
//! Off by default. `--agent-control` starts it on; `Ctrl+A g`
//! ([`Action::ToggleAgentControl`]) turns it on or off while the app runs, and
//! the dock shows it while it is on. The latch gates the app's own BRP
//! methods (`kaijutsu/act`); `kaijutsu/state` is read-only and always
//! answers. BRP's built-in methods and `bevy_brp_extras` (raw key events,
//! text, ECS writes) are outside the latch.

use bevy::prelude::*;

use crate::input::{Action, ActionFired};

/// Whether `kaijutsu/act` is open to an agent.
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq, Reflect)]
#[reflect(Resource)]
pub struct AgentControl {
    pub enabled: bool,
}

pub struct AgentControlPlugin {
    /// The latch's state at startup (`--agent-control`).
    pub enabled: bool,
}

impl Plugin for AgentControlPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AgentControl {
            enabled: self.enabled,
        })
        .register_type::<AgentControl>()
        .add_systems(Update, toggle_agent_control);
    }
}

/// `Ctrl+A g` flips the latch.
pub fn toggle_agent_control(
    mut actions: MessageReader<ActionFired>,
    mut control: ResMut<AgentControl>,
) {
    for ActionFired { action, .. } in actions.read() {
        if matches!(action, Action::ToggleAgentControl) {
            control.enabled = !control.enabled;
            info!(enabled = control.enabled, "agent control toggled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputContext;

    fn app(enabled: bool) -> App {
        let mut app = App::new();
        app.add_message::<ActionFired>()
            .add_plugins(AgentControlPlugin { enabled });
        app
    }

    fn fire(app: &mut App, action: Action) {
        app.world_mut()
            .resource_mut::<Messages<ActionFired>>()
            .write(ActionFired::new(action, InputContext::Global));
    }

    #[test]
    fn starts_off_unless_the_flag_says_otherwise() {
        assert!(!app(false).world().resource::<AgentControl>().enabled);
        assert!(app(true).world().resource::<AgentControl>().enabled);
    }

    #[test]
    fn the_chord_latches_and_unlatches() {
        let mut app = app(false);
        fire(&mut app, Action::ToggleAgentControl);
        app.update();
        assert!(app.world().resource::<AgentControl>().enabled);
        fire(&mut app, Action::ToggleAgentControl);
        app.update();
        assert!(!app.world().resource::<AgentControl>().enabled);
    }

    #[test]
    fn other_actions_leave_the_latch_alone() {
        let mut app = app(false);
        fire(&mut app, Action::HoldQuickContext);
        app.update();
        assert!(!app.world().resource::<AgentControl>().enabled);
    }
}
