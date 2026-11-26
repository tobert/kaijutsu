//! Custom BRP methods for kaijutsu context operations.
//!
//! Registered at runtime via `RemoteMethods` so agents can navigate
//! contexts by ID through the Bevy Remote Protocol.

use bevy::prelude::*;
use bevy_remote::{error_codes, BrpError, BrpResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_control::{AgentControl, RequestOutcome, RequestStatus};
use crate::connection::drift::DriftState;
use crate::input::bindings_config::parse_action;
use crate::input::context::ActiveInputContexts;
use crate::input::map::InputMap;
use crate::input::{Action, ActionFired, FocusArea, InputContext};
use crate::ui::screen::Screen;
use crate::view::boards::{BoardLocation, BoardReader};
use crate::view::components::ContextSwitchRequested;
use crate::view::document::DocumentCache;
use crate::view::heroic::state::HeroicState;
use kaijutsu_types::ContextId;

/// Method name for switching to a context by ID.
pub const SWITCH_CONTEXT_METHOD: &str = "kaijutsu/switch_context";

/// Method name for querying the active context.
pub const ACTIVE_CONTEXT_METHOD: &str = "kaijutsu/active_context";

/// Method name for firing an action as if its binding had been pressed.
pub const ACT_METHOD: &str = "kaijutsu/act";

/// Method name for asking the player to turn agent control on.
pub const REQUEST_CONTROL_METHOD: &str = "kaijutsu/request_control";

/// Method name for the app's input and navigation state (read-only).
pub const STATE_METHOD: &str = "kaijutsu/state";

/// BRP error code for a refused `kaijutsu/act`: agent control is off.
pub const AGENT_CONTROL_OFF: i16 = -23100;

// ── Request / Response types ─────────────────────────────────────────

#[derive(Deserialize)]
struct SwitchContextParams {
    /// UUID string (hyphenated or plain hex).
    context_id: String,
}

#[derive(Deserialize)]
struct ActParams {
    /// An action name, or `Name:payload` — the bindings-config syntax.
    action: String,
}

#[derive(Serialize)]
struct ActResponse {
    action: String,
    /// The binding context the action was stamped with.
    context: String,
}

#[derive(Deserialize)]
struct RequestControlParams {
    /// Why the agent wants control; shown to the player, clipped.
    reason: String,
}

#[derive(Serialize)]
struct RequestControlResponse {
    /// `on` (nothing to ask), `pending` (one is already waiting), or
    /// `requested`. Poll `kaijutsu/state` for the player's answer.
    status: &'static str,
}

#[derive(Serialize)]
struct ControlRequestView {
    /// `pending`, `denied`, or `timed_out`.
    status: &'static str,
    reason: String,
}

#[derive(Serialize)]
struct StateResponse<'a> {
    /// The latest request for control; null when none, or once allowed.
    control_request: Option<ControlRequestView>,
    agent_control: bool,
    screen: String,
    focus: String,
    /// Active binding contexts, lowest priority first.
    contexts: Vec<String>,
    active_context_id: Option<String>,
    heroic: Option<HeroicView>,
    boards: Option<BoardLocation<'a>>,
}

#[derive(Serialize)]
struct HeroicView {
    performers: Vec<ContextId>,
    selected: Option<ContextId>,
}

#[derive(Serialize)]
struct SwitchContextResponse {
    context_id: String,
    was_cached: bool,
}

#[derive(Serialize)]
struct ActiveContextResponse {
    /// Currently rendered context (null if none).
    active_id: Option<String>,
    /// Most-recently-used context IDs, most recent first.
    mru: Vec<MruEntry>,
}

#[derive(Serialize)]
struct MruEntry {
    context_id: String,
    name: String,
    is_active: bool,
}

// ── Handlers ─────────────────────────────────────────────────────────

/// BRP handler: `kaijutsu/switch_context`
///
/// Params: `{ "context_id": "<uuid-or-prefix>" }`
/// Accepts full UUIDs (hyphenated or plain hex) or short hex prefixes.
/// Resolves against the document cache's known contexts.
/// Writes a `ContextSwitchRequested` message, which the existing
/// `handle_context_switch` system picks up next frame.
pub fn handle_switch_context(
    In(params): In<Option<Value>>,
    doc_cache: Res<DocumentCache>,
    drift: Res<DriftState>,
    mut writer: MessageWriter<ContextSwitchRequested>,
) -> BrpResult {
    let SwitchContextParams { context_id: raw } = parse_params(params)?;

    // Try full UUID parse first, fall back to prefix resolution against
    // all known contexts (drift state from the kernel).
    let ctx_id = match ContextId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            let items = drift.contexts.iter().map(|c| {
                let label = if c.label.is_empty() {
                    None
                } else {
                    Some(c.label.as_str())
                };
                (c.id, label)
            });
            kaijutsu_types::resolve_context_prefix(items, &raw).map_err(|e| BrpError {
                code: error_codes::INVALID_PARAMS,
                message: format!("Cannot resolve context_id: {e}"),
                data: None,
            })?
        }
    };

    let was_cached = doc_cache.contains(ctx_id);

    writer.write(ContextSwitchRequested { context_id: ctx_id });

    serde_json::to_value(SwitchContextResponse {
        context_id: ctx_id.to_string(),
        was_cached,
    })
    .map_err(BrpError::internal)
}

/// BRP handler: `kaijutsu/active_context`
///
/// No params. Returns the active context ID and MRU list.
pub fn handle_active_context(
    In(_params): In<Option<Value>>,
    doc_cache: Res<DocumentCache>,
) -> BrpResult {
    let active = doc_cache.active_id();

    let mru: Vec<MruEntry> = doc_cache
        .mru_ids()
        .iter()
        .map(|&id| {
            let name = doc_cache
                .get(id)
                .map(|d| d.context_name.clone())
                .unwrap_or_default();
            MruEntry {
                context_id: id.to_string(),
                name,
                is_active: Some(id) == active,
            }
        })
        .collect();

    serde_json::to_value(ActiveContextResponse {
        active_id: active.map(|id| id.to_string()),
        mru,
    })
    .map_err(BrpError::internal)
}

/// BRP handler: `kaijutsu/act`
///
/// Params: `{ "action": "FocusNextBlock" }`, or `"Name:payload"` for the
/// actions that carry a value (`"JumpSeat:3"`). Fires `ActionFired` as the
/// dispatcher would for a key press: stamped with the active context that
/// binds the action, else `Global` (the prefix chords and wheel scroll are
/// stamped `Global` too). Refused unless agent control is on
/// (`crate::agent_control`); the latch itself is the player's, so
/// `ToggleAgentControl` is refused as well.
pub fn handle_act(
    In(params): In<Option<Value>>,
    control: Res<AgentControl>,
    map: Res<InputMap>,
    active: Res<ActiveInputContexts>,
    mut writer: MessageWriter<ActionFired>,
) -> BrpResult {
    if !control.enabled {
        return Err(BrpError {
            code: AGENT_CONTROL_OFF,
            message: "agent control is off: start the app with --agent-control, or press Ctrl+A g"
                .into(),
            data: None,
        });
    }
    let ActParams { action: raw } = parse_params(params)?;
    let action = parse_action(&raw).map_err(|e| BrpError {
        code: error_codes::INVALID_PARAMS,
        message: format!("Cannot parse action: {e}"),
        data: None,
    })?;
    if matches!(
        action,
        Action::ToggleAgentControl | Action::DenyAgentControl
    ) {
        return Err(BrpError {
            code: error_codes::INVALID_PARAMS,
            message: "the agent-control latch and its answer are the player's; an agent cannot fire them (use kaijutsu/request_control)".into(),
            data: None,
        });
    }
    let context = map
        .bindings
        .iter()
        .find(|b| b.action == action && active.contains(b.context))
        .map(|b| b.context)
        .unwrap_or(InputContext::Global);
    writer.write(ActionFired::new(action, context));
    serde_json::to_value(ActResponse {
        action: raw,
        context: format!("{context:?}"),
    })
    .map_err(BrpError::internal)
}

/// BRP handler: `kaijutsu/request_control`
///
/// Params: `{ "reason": "<why>" }`. Works with control off or on. It only
/// records the request: the dock shows it and the player's `Ctrl+A g` allows
/// it (`crate::agent_control`). Poll `kaijutsu/state` for the answer.
pub fn handle_request_control(
    In(params): In<Option<Value>>,
    time: Res<Time>,
    mut control: ResMut<AgentControl>,
) -> BrpResult {
    let RequestControlParams { reason } = parse_params(params)?;
    if reason.trim().is_empty() {
        return Err(BrpError {
            code: error_codes::INVALID_PARAMS,
            message: "reason must say why control is wanted".into(),
            data: None,
        });
    }
    let status = match control.request(&reason, time.elapsed_secs_f64()) {
        RequestOutcome::On => "on",
        RequestOutcome::Pending => "pending",
        RequestOutcome::Requested => "requested",
    };
    serde_json::to_value(RequestControlResponse { status }).map_err(BrpError::internal)
}

/// BRP handler: `kaijutsu/state`
///
/// No params. Read-only, answers whether or not agent control is on.
pub fn handle_state(
    In(_params): In<Option<Value>>,
    control: Res<AgentControl>,
    screen: Res<State<Screen>>,
    focus: Res<FocusArea>,
    active: Res<ActiveInputContexts>,
    doc_cache: Res<DocumentCache>,
    heroic: Option<Res<HeroicState>>,
    boards: BoardReader,
) -> BrpResult {
    serde_json::to_value(StateResponse {
        control_request: control.request.as_ref().map(|r| ControlRequestView {
            status: match r.status {
                RequestStatus::Pending => "pending",
                RequestStatus::Denied => "denied",
                RequestStatus::TimedOut => "timed_out",
            },
            reason: r.reason.clone(),
        }),
        agent_control: control.enabled,
        screen: format!("{:?}", screen.get()),
        focus: format!("{:?}", *focus),
        contexts: active.0.iter().map(|c| format!("{c:?}")).collect(),
        active_context_id: doc_cache.active_id().map(|id| id.to_string()),
        boards: if *screen.get() == Screen::Boards { boards.location() } else { None },
        heroic: heroic.filter(|_| *screen.get() == Screen::Heroic).map(|scene| HeroicView {
            performers: scene.performer_ids(),
            selected: scene.selected_performer,
        }),
    })
    .map_err(BrpError::internal)
}

// ── Helpers ──────────────────────────────────────────────────────────

fn parse_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, BrpError> {
    match params {
        Some(v) => serde_json::from_value(v).map_err(|e| BrpError {
            code: error_codes::INVALID_PARAMS,
            message: e.to_string(),
            data: None,
        }),
        None => Err(BrpError {
            code: error_codes::INVALID_PARAMS,
            message: "Params not provided".into(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::binding::Binding;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::state::app::StatesPlugin;

    fn app(agent_control: bool, contexts: Vec<InputContext>) -> App {
        let mut app = App::new();
        app.add_plugins(StatesPlugin)
            .add_message::<ActionFired>()
            .init_state::<Screen>()
            .init_resource::<FocusArea>()
            .init_resource::<DocumentCache>()
            .insert_resource(AgentControl::new(agent_control))
            .insert_resource(ActiveInputContexts(contexts))
            .insert_resource(InputMap {
                bindings: vec![Binding::key(
                    KeyCode::KeyJ,
                    InputContext::Navigation,
                    Action::FocusNextBlock,
                    "next block",
                )],
            });
        app
    }

    fn act(app: &mut App, params: Value) -> BrpResult {
        app.world_mut()
            .run_system_once_with(handle_act, Some(params))
            .unwrap()
    }

    fn fired(app: &mut App) -> Vec<ActionFired> {
        app.world_mut()
            .resource_mut::<Messages<ActionFired>>()
            .drain()
            .collect()
    }

    #[test]
    fn act_is_refused_while_agent_control_is_off() {
        let mut app = app(false, vec![InputContext::Global, InputContext::Navigation]);
        let err = act(&mut app, serde_json::json!({"action": "FocusNextBlock"})).unwrap_err();
        assert_eq!(err.code, AGENT_CONTROL_OFF);
        assert!(fired(&mut app).is_empty(), "a refused act fires nothing");
    }

    #[test]
    fn act_fires_stamped_with_the_active_binding_context() {
        let mut app = app(true, vec![InputContext::Global, InputContext::Navigation]);
        let ok = act(&mut app, serde_json::json!({"action": "FocusNextBlock"})).unwrap();
        assert_eq!(ok["context"], "Navigation");
        let messages = fired(&mut app);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].action, Action::FocusNextBlock);
        assert_eq!(messages[0].context, InputContext::Navigation);
    }

    /// An action bound only in a context that is not active gets `Global`,
    /// the way the prefix chords do — it never borrows the other surface's.
    #[test]
    fn act_stamps_global_when_no_active_context_binds_the_action() {
        let mut app = app(true, vec![InputContext::Global]);
        act(&mut app, serde_json::json!({"action": "FocusNextBlock"})).unwrap();
        assert_eq!(fired(&mut app)[0].context, InputContext::Global);
    }

    #[test]
    fn act_carries_a_payload() {
        let mut app = app(true, vec![InputContext::Global]);
        act(&mut app, serde_json::json!({"action": "JumpSeat:3"})).unwrap();
        assert_eq!(fired(&mut app)[0].action, Action::JumpSeat(3));
    }

    #[test]
    fn act_rejects_an_unknown_action() {
        let mut app = app(true, vec![InputContext::Global]);
        let err = act(&mut app, serde_json::json!({"action": "NoSuchAction"})).unwrap_err();
        assert_eq!(err.code, error_codes::INVALID_PARAMS);
        assert!(fired(&mut app).is_empty());
    }

    #[test]
    fn act_cannot_flip_the_players_latch() {
        let mut app = app(true, vec![InputContext::Global]);
        let err = act(&mut app, serde_json::json!({"action": "ToggleAgentControl"})).unwrap_err();
        assert_eq!(err.code, error_codes::INVALID_PARAMS);
        assert!(fired(&mut app).is_empty());
    }

    #[test]
    fn act_cannot_answer_the_players_request() {
        let mut app = app(true, vec![InputContext::Global]);
        let err = act(&mut app, serde_json::json!({"action": "DenyAgentControl"})).unwrap_err();
        assert_eq!(err.code, error_codes::INVALID_PARAMS);
        assert!(fired(&mut app).is_empty());
    }

    fn request(app: &mut App, params: Value) -> BrpResult {
        app.world_mut()
            .run_system_once_with(handle_request_control, Some(params))
            .unwrap()
    }

    fn state(app: &mut App) -> Value {
        app.world_mut()
            .run_system_once_with(handle_state, None)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_guide_can_read_its_board_position_with_control_off() {
        let mut app = app(false, vec![InputContext::Boards]);
        app.add_plugins(crate::view::boards::BoardsPlugin);
        app.insert_resource(State::new(Screen::Boards));
        let snapshot = state(&mut app);
        assert_eq!(snapshot["boards"]["layer"], "Boards");
        assert_eq!(snapshot["boards"]["loading"], true);
        assert!(snapshot["boards"]["post"].is_null());
        assert_eq!(snapshot["agent_control"], false);
    }

    /// A request works with the latch off, records itself, and leaves the
    /// latch alone: only the player's key turns it on.
    #[test]
    fn a_request_is_recorded_and_shown_in_state_without_opening_control() {
        let mut app = app(false, vec![InputContext::Global]);
        app.insert_resource(Time::<()>::default());
        let ok = request(&mut app, serde_json::json!({"reason": "drive the smoke test"})).unwrap();
        assert_eq!(ok["status"], "requested");
        let state = state(&mut app);
        assert_eq!(state["agent_control"], false);
        assert_eq!(state["control_request"]["status"], "pending");
        assert_eq!(state["control_request"]["reason"], "drive the smoke test");
        // Still refused.
        let err = act(&mut app, serde_json::json!({"action": "FocusNextBlock"})).unwrap_err();
        assert_eq!(err.code, AGENT_CONTROL_OFF);
    }

    #[test]
    fn a_repeat_request_says_pending_and_an_open_latch_says_on() {
        let mut app = app(false, vec![InputContext::Global]);
        app.insert_resource(Time::<()>::default());
        request(&mut app, serde_json::json!({"reason": "one"})).unwrap();
        let again = request(&mut app, serde_json::json!({"reason": "two"})).unwrap();
        assert_eq!(again["status"], "pending");
        let mut open = self::app(true, vec![InputContext::Global]);
        open.insert_resource(Time::<()>::default());
        let on = request(&mut open, serde_json::json!({"reason": "x"})).unwrap();
        assert_eq!(on["status"], "on");
    }

    #[test]
    fn a_request_without_a_reason_is_refused() {
        let mut app = app(false, vec![InputContext::Global]);
        app.insert_resource(Time::<()>::default());
        for bad in [serde_json::json!({"reason": "  "}), serde_json::json!({})] {
            let err = request(&mut app, bad).unwrap_err();
            assert_eq!(err.code, error_codes::INVALID_PARAMS);
        }
        assert!(state(&mut app)["control_request"].is_null());
    }

    #[test]
    fn state_answers_with_the_latch_off() {
        let mut app = app(false, vec![InputContext::Global, InputContext::TextInput]);
        let state = app
            .world_mut()
            .run_system_once_with(handle_state, None)
            .unwrap()
            .unwrap();
        assert_eq!(state["agent_control"], false);
        assert_eq!(state["contexts"], serde_json::json!(["Global", "TextInput"]));
        assert_eq!(state["focus"], "Compose");
        assert!(state["active_context_id"].is_null());
    }
}
