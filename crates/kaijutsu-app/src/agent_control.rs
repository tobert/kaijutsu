//! The agent-control latch: whether an agent may fire actions over BRP.
//!
//! Off by default. `--agent-control` starts it on; `Ctrl+A g`
//! ([`Action::ToggleAgentControl`]) turns it on or off while the app runs, and
//! the dock shows it while it is on. The latch gates the app's own BRP
//! methods (`kaijutsu/act`); `kaijutsu/state` is read-only and always
//! answers. BRP's built-in methods and `bevy_brp_extras` (raw key events,
//! text, ECS writes) are outside the latch.
//!
//! An agent can ask for the latch with `kaijutsu/request_control`. The request
//! shows in the dock's hints line; `Ctrl+A g` allows it, `Ctrl+A G` refuses it,
//! and it expires after [`REQUEST_TTL_SECS`]. The player's key is the only way
//! it becomes control.

use bevy::prelude::*;

use crate::input::{Action, ActionFired};

/// How long a request waits for the player before it counts as refused.
pub const REQUEST_TTL_SECS: f64 = 60.0;

/// The longest reason kept, in characters. The reason is text from an agent
/// shown to the player, so it is clipped, not interpreted.
pub const REASON_MAX_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Reflect)]
pub enum RequestStatus {
    /// Waiting for the player.
    Pending,
    /// The player pressed `Ctrl+A G`.
    Denied,
    /// [`REQUEST_TTL_SECS`] passed with no answer.
    TimedOut,
}

/// An agent's ask for control. An allowed request is not kept: the latch
/// turning on is the answer.
#[derive(Clone, Debug, PartialEq, Reflect)]
pub struct ControlRequest {
    pub reason: String,
    /// `Time::elapsed_secs_f64` when it was made.
    pub asked_at: f64,
    pub status: RequestStatus,
}

/// What [`AgentControl::request`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Control is already on; nothing to ask.
    On,
    /// A request is already waiting; this one added nothing.
    Pending,
    /// A new request is now waiting for the player.
    Requested,
}

/// Whether `kaijutsu/act` is open to an agent, and the request for it.
#[derive(Resource, Default, Clone, Debug, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct AgentControl {
    pub enabled: bool,
    pub request: Option<ControlRequest>,
}

impl AgentControl {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            request: None,
        }
    }

    /// Record an agent's request, unless control is on or one is waiting.
    pub fn request(&mut self, reason: &str, now: f64) -> RequestOutcome {
        if self.enabled {
            return RequestOutcome::On;
        }
        if self.pending_reason().is_some() {
            return RequestOutcome::Pending;
        }
        self.request = Some(ControlRequest {
            reason: reason.trim().chars().take(REASON_MAX_CHARS).collect(),
            asked_at: now,
            status: RequestStatus::Pending,
        });
        RequestOutcome::Requested
    }

    /// The reason of the request waiting for the player, if one is.
    pub fn pending_reason(&self) -> Option<&str> {
        match &self.request {
            Some(r) if r.status == RequestStatus::Pending => Some(r.reason.as_str()),
            _ => None,
        }
    }
}

pub struct AgentControlPlugin {
    /// The latch's state at startup (`--agent-control`).
    pub enabled: bool,
}

impl Plugin for AgentControlPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AgentControl::new(self.enabled))
            .register_type::<AgentControl>()
            .add_systems(Update, (answer_control_request, expire_control_request));
    }
}

/// `Ctrl+A g` flips the latch (turning it on answers any request); `Ctrl+A G`
/// refuses the pending request.
pub fn answer_control_request(
    mut actions: MessageReader<ActionFired>,
    mut control: ResMut<AgentControl>,
) {
    for ActionFired { action, .. } in actions.read() {
        match action {
            Action::ToggleAgentControl => {
                control.enabled = !control.enabled;
                control.request = None;
                info!(enabled = control.enabled, "agent control toggled");
            }
            Action::DenyAgentControl => {
                if let Some(request) = control
                    .request
                    .as_mut()
                    .filter(|r| r.status == RequestStatus::Pending)
                {
                    request.status = RequestStatus::Denied;
                    info!("agent control request refused");
                }
            }
            _ => {}
        }
    }
}

/// A request nobody answered becomes `TimedOut`.
pub fn expire_control_request(time: Res<Time>, mut control: ResMut<AgentControl>) {
    let now = time.elapsed_secs_f64();
    let expired = matches!(
        &control.request,
        Some(r) if r.status == RequestStatus::Pending && now - r.asked_at >= REQUEST_TTL_SECS
    );
    if expired && let Some(request) = control.request.as_mut() {
        request.status = RequestStatus::TimedOut;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputContext;
    use std::time::Duration;

    fn app(enabled: bool) -> App {
        let mut app = App::new();
        app.add_message::<ActionFired>()
            .insert_resource(Time::<()>::default())
            .add_plugins(AgentControlPlugin { enabled });
        app
    }

    fn fire(app: &mut App, action: Action) {
        app.world_mut()
            .resource_mut::<Messages<ActionFired>>()
            .write(ActionFired::new(action, InputContext::Global));
    }

    fn control(app: &App) -> &AgentControl {
        app.world().resource::<AgentControl>()
    }

    fn advance(app: &mut App, secs: f64) {
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_secs_f64(secs));
    }

    #[test]
    fn starts_off_unless_the_flag_says_otherwise() {
        assert!(!control(&app(false)).enabled);
        assert!(control(&app(true)).enabled);
    }

    #[test]
    fn the_chord_latches_and_unlatches() {
        let mut app = app(false);
        fire(&mut app, Action::ToggleAgentControl);
        app.update();
        assert!(control(&app).enabled);
        fire(&mut app, Action::ToggleAgentControl);
        app.update();
        assert!(!control(&app).enabled);
    }

    #[test]
    fn other_actions_leave_the_latch_alone() {
        let mut app = app(false);
        fire(&mut app, Action::HoldQuickContext);
        app.update();
        assert!(!control(&app).enabled);
    }

    #[test]
    fn a_request_waits_and_never_turns_control_on_by_itself() {
        let mut c = AgentControl::new(false);
        assert_eq!(c.request("run the smoke test", 1.0), RequestOutcome::Requested);
        assert_eq!(c.pending_reason(), Some("run the smoke test"));
        assert!(!c.enabled);
    }

    #[test]
    fn a_second_request_adds_nothing_and_an_open_latch_needs_none() {
        let mut c = AgentControl::new(false);
        c.request("first", 1.0);
        assert_eq!(c.request("second", 2.0), RequestOutcome::Pending);
        assert_eq!(c.pending_reason(), Some("first"));
        assert_eq!(AgentControl::new(true).request("x", 0.0), RequestOutcome::On);
    }

    #[test]
    fn a_long_reason_is_clipped() {
        let mut c = AgentControl::new(false);
        c.request(&"x".repeat(500), 0.0);
        assert_eq!(c.pending_reason().unwrap().chars().count(), REASON_MAX_CHARS);
    }

    #[test]
    fn allowing_turns_control_on_and_clears_the_request() {
        let mut app = app(false);
        app.world_mut().resource_mut::<AgentControl>().request("why", 0.0);
        fire(&mut app, Action::ToggleAgentControl);
        app.update();
        assert!(control(&app).enabled);
        assert_eq!(control(&app).request, None);
    }

    #[test]
    fn refusing_keeps_control_off_and_says_denied() {
        let mut app = app(false);
        app.world_mut().resource_mut::<AgentControl>().request("why", 0.0);
        fire(&mut app, Action::DenyAgentControl);
        app.update();
        assert!(!control(&app).enabled);
        assert_eq!(control(&app).request.as_ref().unwrap().status, RequestStatus::Denied);
        // A refused request does not block the next one.
        assert_eq!(
            app.world_mut().resource_mut::<AgentControl>().request("again", 5.0),
            RequestOutcome::Requested
        );
    }

    #[test]
    fn a_request_nobody_answers_times_out() {
        let mut app = app(false);
        app.world_mut().resource_mut::<AgentControl>().request("why", 0.0);
        advance(&mut app, REQUEST_TTL_SECS - 1.0);
        app.update();
        assert_eq!(control(&app).request.as_ref().unwrap().status, RequestStatus::Pending);
        advance(&mut app, 2.0);
        app.update();
        assert_eq!(control(&app).request.as_ref().unwrap().status, RequestStatus::TimedOut);
        assert!(!control(&app).enabled);
    }
}
