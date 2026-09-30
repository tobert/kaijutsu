//! Harbor-shape invariants: what Harbor's ACP runner and trajectory builder
//! need from any ACP agent, checked on every scenario's transcript.
//!
//! Harbor (the Terminal-Bench harness) drives an agent through one
//! `session/prompt` and builds its trajectory from the `session/update`
//! stream. Each invariant here names what Harbor reads; `docs/acp-fleet.md`,
//! "Harbor shape" lists the Harbor source each one rests on. A failure names
//! the invariant and the offending event.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::client::{Arrival, PermissionRecord};

/// Every `stopReason` ACP v1 defines.
pub const STOP_REASONS: &[&str] = &["end_turn", "max_tokens", "max_turn_requests", "refusal", "cancelled"];

/// The update kinds that carry a model's work. Harbor stops reading when the
/// prompt returns, so one of these after the response is lost to it.
const WORK_UPDATES: &[&str] = &["agent_message_chunk", "agent_thought_chunk", "tool_call", "tool_call_update"];

pub const AGENT_INFO: &str = "agent-info";
pub const TOOL_CALL_FIRST: &str = "tool-call-first";
pub const TOOL_CALL_UNIQUE: &str = "tool-call-unique";
pub const TOOL_CALL_TITLE: &str = "tool-call-title";
pub const TOOL_CALL_INPUT: &str = "tool-call-input";
pub const TOOL_CALL_SETTLES: &str = "tool-call-settles";
pub const PERMISSION_ALLOW_OPTION: &str = "permission-allow-option";
pub const PERMISSION_TOOL_CALL: &str = "permission-tool-call";
pub const STOP_REASON: &str = "stop-reason";
pub const RUN_ENDS_AT_RESPONSE: &str = "run-ends-at-response";
pub const USAGE_COST: &str = "usage-cost";

/// Every invariant, in the order they are checked.
pub const INVARIANTS: &[&str] = &[
    AGENT_INFO,
    TOOL_CALL_FIRST,
    TOOL_CALL_UNIQUE,
    TOOL_CALL_TITLE,
    TOOL_CALL_INPUT,
    TOOL_CALL_SETTLES,
    PERMISSION_ALLOW_OPTION,
    PERMISSION_TOOL_CALL,
    STOP_REASON,
    RUN_ENDS_AT_RESPONSE,
    USAGE_COST,
];

/// The prefix of every invariant failure, so a `known_gap` can name one:
/// `fails = ["harbor shape run-ends-at-response"]`.
pub fn prefix(invariant: &str) -> String {
    format!("harbor shape {invariant}")
}

/// One prompt as it went over the wire.
#[derive(Debug, Clone)]
pub struct PromptWire {
    pub label: String,
    /// Where the prompt was sent.
    pub sent: Arrival,
    /// The `session/prompt` result.
    pub response: Value,
    /// Where its response arrived.
    pub answered: Arrival,
    /// Where the runner stopped reading for this prompt: after the quiet wait.
    pub ended: Arrival,
}

/// Everything one scenario put on the wire, in arrival order.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub initialize: Value,
    pub session_new: Value,
    pub updates: Vec<Value>,
    pub permissions: Vec<PermissionRecord>,
    pub prompts: Vec<PromptWire>,
}

/// The verdict on one invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub invariant: &'static str,
    /// Whether the transcript had anything the invariant applies to. An
    /// invariant with nothing to check cannot show that a gap is fixed.
    pub exercised: bool,
    pub failures: Vec<String>,
}

/// Check every invariant against `transcript`.
pub fn check(transcript: &Transcript) -> Vec<Check> {
    let mut found: Vec<(&'static str, String)> = Vec::new();
    let mut exercised: HashSet<&'static str> = HashSet::new();
    let mut fail = |invariant: &'static str, what: String, event: &Value| {
        found.push((invariant, format!("{}: {what}; event: {}", prefix(invariant), brief(event))));
    };

    let info = transcript.initialize.get("agentInfo");
    let named = |key: &str| info.and_then(|i| i.get(key)).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if !(named("name") && named("version")) {
        fail(AGENT_INFO, "initialize has no agentInfo with a name and version".into(), &transcript.initialize);
    }

    // Tool calls, by id: the index of the announcing `tool_call`, whether
    // any event carried rawInput, and the last status seen.
    struct Call {
        announced: usize,
        has_input: bool,
        status: Option<String>,
    }
    let mut calls: HashMap<String, Call> = HashMap::new();
    for (index, params) in transcript.updates.iter().enumerate() {
        let update = params.get("update").unwrap_or(&Value::Null);
        let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
        match kind {
            "tool_call" | "tool_call_update" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str).filter(|id| !id.is_empty()) else {
                    fail(TOOL_CALL_FIRST, format!("a {kind} has no toolCallId"), params);
                    continue;
                };
                let raw_input = update.get("rawInput");
                if let Some(input) = raw_input
                    && !input.is_object()
                {
                    fail(TOOL_CALL_INPUT, format!("rawInput for {id} is not a JSON object"), params);
                }
                if kind == "tool_call" {
                    if calls.contains_key(id) {
                        fail(TOOL_CALL_UNIQUE, format!("a second tool_call announces {id}"), params);
                        continue;
                    }
                    let title = update.get("title").and_then(Value::as_str).unwrap_or("").trim();
                    if title.is_empty() {
                        fail(TOOL_CALL_TITLE, format!("the tool_call announcing {id} has no title"), params);
                    }
                    calls.insert(
                        id.to_string(),
                        Call {
                            announced: index,
                            has_input: raw_input.is_some(),
                            status: update.get("status").and_then(Value::as_str).map(str::to_string),
                        },
                    );
                } else {
                    let Some(call) = calls.get_mut(id) else {
                        fail(TOOL_CALL_FIRST, format!("a tool_call_update names {id} before any tool_call announces it"), params);
                        continue;
                    };
                    call.has_input |= raw_input.is_some();
                    if let Some(status) = update.get("status").and_then(Value::as_str) {
                        call.status = Some(status.to_string());
                    }
                    if let Some(title) = update.get("title").and_then(Value::as_str)
                        && title.trim().is_empty()
                    {
                        fail(TOOL_CALL_TITLE, format!("a tool_call_update clears the title of {id}"), params);
                    }
                }
            }
            "usage_update" => {
                if let Some(cost) = update.get("cost") {
                    exercised.insert(USAGE_COST);
                    let usd = cost.get("currency").and_then(Value::as_str).is_some_and(|c| c.eq_ignore_ascii_case("USD"));
                    if !usd || !cost.get("amount").is_some_and(Value::is_number) {
                        fail(USAGE_COST, "usage_update.cost is not {currency: \"USD\", amount: <number>}".into(), params);
                    }
                }
            }
            _ => {}
        }
    }
    for (id, call) in &calls {
        if !call.has_input {
            fail(TOOL_CALL_INPUT, format!("no event for {id} carries rawInput"), &transcript.updates[call.announced]);
        }
    }
    if !calls.is_empty() {
        exercised.extend([TOOL_CALL_FIRST, TOOL_CALL_UNIQUE, TOOL_CALL_TITLE, TOOL_CALL_INPUT]);
    }

    for (n, record) in transcript.permissions.iter().enumerate() {
        let request = &record.params;
        let label = format!("permission request {}", n + 1);
        let offers_allow = request
            .get("options")
            .and_then(Value::as_array)
            .is_some_and(|options| {
                options.iter().any(|o| matches!(o.get("kind").and_then(Value::as_str), Some("allow_once" | "allow_always")))
            });
        if !offers_allow {
            fail(PERMISSION_ALLOW_OPTION, format!("{label} offers no allow_once or allow_always option"), request);
        }
        let id = request.pointer("/toolCall/toolCallId").and_then(Value::as_str).unwrap_or("");
        let known = calls.get(id).is_some_and(|call| call.announced < record.updates_seen);
        if !known {
            fail(
                PERMISSION_TOOL_CALL,
                format!("{label} names toolCallId {id:?}, which no earlier tool_call announced"),
                request,
            );
        }
    }
    if !transcript.permissions.is_empty() {
        exercised.extend([PERMISSION_ALLOW_OPTION, PERMISSION_TOOL_CALL]);
    }

    for prompt in &transcript.prompts {
        let stop = prompt.response.get("stopReason").and_then(Value::as_str);
        if !stop.is_some_and(|s| STOP_REASONS.contains(&s)) {
            fail(STOP_REASON, format!("{}: stopReason {stop:?} is not one ACP v1 defines", prompt.label), &prompt.response);
        }

        // A call announced during this prompt must be settled by its response.
        let announced: Vec<(&String, &Call)> = calls
            .iter()
            .filter(|(_, c)| c.announced >= prompt.sent.updates && c.announced < prompt.answered.updates)
            .collect();
        for (id, call) in announced {
            let at_response = last_status(&transcript.updates[..prompt.answered.updates], id);
            if !matches!(at_response.as_deref(), Some("completed" | "failed")) {
                fail(
                    TOOL_CALL_SETTLES,
                    format!(
                        "{}: tool call {id} is {} when the prompt's response arrives, and ends {}",
                        prompt.label,
                        at_response.as_deref().unwrap_or("without a status"),
                        call.status.as_deref().unwrap_or("without a status")
                    ),
                    &transcript.updates[call.announced],
                );
            }
        }

        let late_updates = &transcript.updates[prompt.answered.updates..prompt.ended.updates];
        if let Some(late) = late_updates.iter().find(|u| {
            u.pointer("/update/sessionUpdate").and_then(Value::as_str).is_some_and(|k| WORK_UPDATES.contains(&k))
        }) {
            let count = late_updates
                .iter()
                .filter(|u| u.pointer("/update/sessionUpdate").and_then(Value::as_str).is_some_and(|k| WORK_UPDATES.contains(&k)))
                .count();
            fail(
                RUN_ENDS_AT_RESPONSE,
                format!("{}: {count} update(s) of model work arrived after the prompt's response; the first", prompt.label),
                late,
            );
        }
        if let Some(late) = transcript.permissions.get(prompt.answered.permissions..prompt.ended.permissions)
            && let Some(first) = late.first()
        {
            fail(
                RUN_ENDS_AT_RESPONSE,
                format!("{}: {} permission request(s) arrived after the prompt's response; the first", prompt.label, late.len()),
                &first.params,
            );
        }
    }
    exercised.insert(AGENT_INFO);
    if !transcript.prompts.is_empty() {
        exercised.extend([STOP_REASON, RUN_ENDS_AT_RESPONSE]);
        if !calls.is_empty() {
            exercised.insert(TOOL_CALL_SETTLES);
        }
    }

    INVARIANTS
        .iter()
        .map(|&invariant| {
            let mut failures: Vec<String> =
                found.iter().filter(|(i, _)| *i == invariant).map(|(_, f)| f.clone()).collect();
            let mut seen = HashSet::new();
            failures.retain(|f| seen.insert(f.clone()));
            Check { invariant, exercised: exercised.contains(invariant) || !failures.is_empty(), failures }
        })
        .collect()
}

/// The last status `updates` report for tool call `id`.
fn last_status(updates: &[Value], id: &str) -> Option<String> {
    updates.iter().rev().find_map(|params| {
        let update = params.get("update")?;
        if update.get("toolCallId").and_then(Value::as_str) != Some(id) {
            return None;
        }
        update.get("status").and_then(Value::as_str).map(str::to_string)
    })
}

/// An event, cut to a length that fits a failure line.
fn brief(event: &Value) -> String {
    const MAX: usize = 400;
    let text = event.to_string();
    if text.len() <= MAX {
        return text;
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(body: Value) -> Value {
        json!({"sessionId": "s", "update": body})
    }

    fn call(id: &str) -> Value {
        update(json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": "shell_write", "kind": "edit",
            "status": "in_progress", "rawInput": {"command": "true"}}))
    }

    fn settle(id: &str, status: &str) -> Value {
        update(json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status}))
    }

    fn text(t: &str) -> Value {
        update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}}))
    }

    fn permission(id: &str, updates_seen: usize) -> PermissionRecord {
        PermissionRecord {
            params: json!({"sessionId": "s", "toolCall": {"toolCallId": id, "title": "ask"}, "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
            ]}),
            answer: None,
            option_id: None,
            problem: None,
            updates_seen,
        }
    }

    fn at(updates: usize, permissions: usize) -> Arrival {
        Arrival { updates, permissions }
    }

    /// One prompt whose response arrives after `answered` updates, read
    /// until `ended` updates.
    fn transcript(updates: Vec<Value>, permissions: Vec<PermissionRecord>, answered: usize) -> Transcript {
        let ended = updates.len();
        let permissions_len = permissions.len();
        Transcript {
            initialize: json!({"protocolVersion": 1, "agentInfo": {"name": "agent", "version": "1"}}),
            session_new: json!({"sessionId": "s"}),
            updates,
            permissions,
            prompts: vec![PromptWire {
                label: "prompt 1".into(),
                sent: at(0, 0),
                response: json!({"stopReason": "end_turn"}),
                answered: at(answered, permissions_len),
                ended: at(ended, permissions_len),
            }],
        }
    }

    fn failures(t: &Transcript) -> Vec<String> {
        check(t).into_iter().flat_map(|c| c.failures).collect()
    }

    fn failing(t: &Transcript) -> Vec<&'static str> {
        check(t).into_iter().filter(|c| !c.failures.is_empty()).map(|c| c.invariant).collect()
    }

    #[test]
    fn a_well_formed_transcript_passes_every_invariant() {
        let updates = vec![call("a"), settle("a", "completed"), text("done")];
        let t = transcript(updates, vec![permission("a", 1)], 3);
        assert_eq!(failures(&t), Vec::<String>::new());
        let exercised: Vec<&str> = check(&t).iter().filter(|c| c.exercised).map(|c| c.invariant).collect();
        assert!(!exercised.contains(&USAGE_COST), "no cost was reported, so the cost shape was not checked");
        assert!(exercised.contains(&PERMISSION_TOOL_CALL));
    }

    #[test]
    fn an_update_before_its_tool_call_fails_and_names_the_event() {
        let t = transcript(vec![settle("a", "completed"), call("a"), settle("a", "completed")], vec![], 3);
        let all = failures(&t);
        assert_eq!(failing(&t), vec![TOOL_CALL_FIRST], "{all:#?}");
        assert!(all[0].starts_with("harbor shape tool-call-first: ") && all[0].contains("\"toolCallId\":\"a\""), "{all:#?}");
    }

    #[test]
    fn a_repeated_tool_call_id_fails() {
        let mut again = call("a");
        again["update"]["status"] = json!("completed");
        let t = transcript(vec![call("a"), settle("a", "completed"), again], vec![], 3);
        assert_eq!(failing(&t), vec![TOOL_CALL_UNIQUE]);
    }

    #[test]
    fn a_tool_call_with_no_title_or_no_input_fails() {
        let bare = update(json!({"sessionUpdate": "tool_call", "toolCallId": "a", "status": "in_progress"}));
        let t = transcript(vec![bare, settle("a", "completed")], vec![], 2);
        assert_eq!(failing(&t), vec![TOOL_CALL_TITLE, TOOL_CALL_INPUT]);
    }

    #[test]
    fn input_on_a_later_update_counts() {
        let bare = update(json!({"sessionUpdate": "tool_call", "toolCallId": "a", "title": "t"}));
        let late = update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed",
            "rawInput": {"x": 1}}));
        assert_eq!(failures(&transcript(vec![bare, late], vec![], 2)), Vec::<String>::new());
    }

    #[test]
    fn a_tool_call_still_running_at_the_response_fails() {
        let t = transcript(vec![call("a"), settle("a", "pending"), settle("a", "completed")], vec![], 2);
        let all = failures(&t);
        assert!(failing(&t).contains(&TOOL_CALL_SETTLES), "{all:#?}");
        assert!(all.iter().any(|f| f.contains("is pending when the prompt's response arrives")), "{all:#?}");
    }

    #[test]
    fn a_permission_request_must_name_an_earlier_tool_call_and_offer_allow() {
        let unknown = transcript(vec![call("a"), settle("a", "completed")], vec![permission("ask-1", 1)], 2);
        assert_eq!(failing(&unknown), vec![PERMISSION_TOOL_CALL]);
        let early = transcript(vec![call("a"), settle("a", "completed")], vec![permission("a", 0)], 2);
        assert_eq!(failing(&early), vec![PERMISSION_TOOL_CALL]);
        let mut deny_only = permission("a", 1);
        deny_only.params["options"] = json!([{"optionId": "deny", "name": "Deny", "kind": "reject_once"}]);
        let t = transcript(vec![call("a"), settle("a", "completed")], vec![deny_only], 2);
        assert_eq!(failing(&t), vec![PERMISSION_ALLOW_OPTION]);
    }

    #[test]
    fn a_nonstandard_stop_reason_fails() {
        let mut t = transcript(vec![text("hi")], vec![], 1);
        t.prompts[0].response = json!({"stopReason": "stream_error"});
        assert_eq!(failing(&t), vec![STOP_REASON]);
    }

    #[test]
    fn work_after_the_response_fails_but_other_updates_do_not() {
        let usage = update(json!({"sessionUpdate": "usage_update", "used": 10, "size": 100}));
        let quiet = transcript(vec![text("hi"), usage], vec![], 1);
        assert_eq!(failures(&quiet), Vec::<String>::new());
        let late = transcript(vec![text("hi"), text("the background job finished")], vec![], 1);
        let all = failures(&late);
        assert_eq!(failing(&late), vec![RUN_ENDS_AT_RESPONSE]);
        assert!(all[0].contains("1 update(s) of model work") && all[0].contains("background job"), "{all:#?}");
    }

    #[test]
    fn a_permission_request_after_the_response_fails() {
        let mut t = transcript(vec![call("a"), settle("a", "completed")], vec![permission("a", 1)], 2);
        t.prompts[0].answered.permissions = 0;
        assert_eq!(failing(&t), vec![RUN_ENDS_AT_RESPONSE]);
    }

    #[test]
    fn a_reported_cost_must_be_usd() {
        let usd = update(json!({"sessionUpdate": "usage_update", "used": 1, "size": 2,
            "cost": {"amount": 0.25, "currency": "USD"}}));
        let t = transcript(vec![usd], vec![], 1);
        assert_eq!(failures(&t), Vec::<String>::new());
        assert!(check(&t).iter().any(|c| c.invariant == USAGE_COST && c.exercised));
        let yen = update(json!({"sessionUpdate": "usage_update", "used": 1, "size": 2,
            "cost": {"amount": 30, "currency": "JPY"}}));
        assert_eq!(failing(&transcript(vec![yen], vec![], 1)), vec![USAGE_COST]);
    }

    #[test]
    fn initialize_must_name_the_agent() {
        let mut t = transcript(vec![], vec![], 0);
        t.initialize = json!({"protocolVersion": 1});
        assert_eq!(failing(&t), vec![AGENT_INFO]);
    }
}
