//! Request and response types council v2 adds or changes
//! (`docs/council-api-v2.md`).
//!
//! Requests serialize exactly the draft's fields and leave out absent
//! optionals. Responses are strict about required fields and types and
//! tolerate fields the draft does not name, as v1's are. Snapshot ids keep
//! v1's form, `snap:` and 64 lowercase hex digits.
//!
//! Where the draft is silent, these types choose, and each choice is named
//! on its type: a reply tool call carries the `id` the server gave it, a
//! generation's `done` reports `head` and `snapshots` as a `PUT` reply does,
//! its `token` events are `/mk/v1`'s, and a `reads` reply is `reads` with
//! each read's `logprobs`, `mass`, and `top`.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

use crate::council::wire::{self as v1, ContextId, ContextRef, InvalidRequest, SnapshotId, SpecId};
use crate::generate::{Finish, Sample, TokenEvent, Tool};
use crate::json::Json;

fn is_false(b: &bool) -> bool {
    !*b
}

/// Accepts `null` or a value but, unlike a plain `Option`, requires the member.
fn required_nullable<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de)
}

// ---- messages

/// Who wrote a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool call in an assistant message. `id` is what a `tool` message's
/// `tool_call_id` names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// A JSON object, in the member order the model wrote.
    pub arguments: Json,
}

/// One message of a held context, in the model's chat shape. The server
/// renders it with its own template.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// An assistant message's thinking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// An assistant message's tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// A `tool` message's answer to this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Keep a snapshot boundary at the end of this message. Left out when
    /// false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub snap: bool,
}

impl Message {
    fn text(role: Role, content: impl Into<String>) -> Self {
        Message { role, content: Some(content.into()), reasoning: None, tool_calls: Vec::new(), tool_call_id: None, snap: false }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    /// An assistant message with no thinking and no calls.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    /// A tool result answering the call `call_id`.
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message { tool_call_id: Some(call_id.into()), ..Self::text(Role::Tool, content) }
    }

    /// The same message with a snapshot boundary at its end.
    pub fn snapped(self) -> Self {
        Message { snap: true, ..self }
    }
}

/// Refuses messages the draft refuses: `reasoning` or `tool_calls` off an
/// assistant message, a `tool` message without `tool_call_id` or another
/// message with one, and a call with an empty id or name or arguments that
/// are not an object. A `system` message may come anywhere: after the first
/// message it is system text at that point in the conversation.
pub fn validate_messages(messages: &[Message]) -> Result<(), InvalidRequest> {
    let bad = |i: usize, m: &str| Err(InvalidRequest(format!("message {i}: {m}")));
    for (i, m) in messages.iter().enumerate() {
        if m.role != Role::Assistant && m.reasoning.is_some() {
            return bad(i, "only an assistant message carries reasoning");
        }
        if m.role != Role::Assistant && !m.tool_calls.is_empty() {
            return bad(i, "only an assistant message carries tool_calls");
        }
        match (m.role, &m.tool_call_id) {
            (Role::Tool, None) => return bad(i, "a tool message names the tool_call_id it answers"),
            (Role::Tool, Some(id)) if id.is_empty() => return bad(i, "tool_call_id is empty"),
            (Role::Tool, Some(_)) => {}
            (_, Some(_)) => return bad(i, "only a tool message carries tool_call_id"),
            (_, None) => {}
        }
        for call in &m.tool_calls {
            if call.id.is_empty() || call.name.is_empty() {
                return bad(i, "a tool call has an empty id or name");
            }
            if !matches!(call.arguments, Json::Object(_)) {
                return Err(InvalidRequest(format!("message {i}: tool call {:?} arguments are not an object", call.name)));
            }
        }
    }
    Ok(())
}

// ---- contexts

/// `fork` in a `PUT`: the new context's messages extend `at`, a snapshot of
/// `context`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fork {
    pub context: ContextId,
    pub at: SnapshotId,
}

/// The body of `PUT /council/v2/contexts/{id}`: the whole context.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextPut {
    /// Absent: the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// A tool in `tools` whose call receives a decision's case as its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    pub messages: Vec<Message>,
    /// Requires the `fork` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<Fork>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm: Option<Vec<SpecId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

impl ContextPut {
    /// v1's body as v2's: the system message first, then each turn as a
    /// message with its `snap` and `reasoning`. A v1 `tool` turn names no
    /// call, so it has no v2 form and is refused.
    pub fn from_v1(body: v1::ContextPut) -> Result<Self, InvalidRequest> {
        let mut messages = vec![Message::system(body.system)];
        for (i, turn) in body.turns.into_iter().enumerate() {
            let role = match turn.role {
                v1::Role::User => Role::User,
                v1::Role::Assistant => Role::Assistant,
                v1::Role::Tool => {
                    return Err(InvalidRequest(format!("turn {i} is a v1 tool turn, which names no tool call")));
                }
            };
            messages.push(Message {
                role,
                content: Some(turn.content),
                reasoning: turn.reasoning,
                tool_calls: Vec::new(),
                tool_call_id: None,
                snap: turn.snap,
            });
        }
        Ok(ContextPut {
            messages,
            pin: body.pin,
            persist: body.persist,
            warm: body.warm,
            dry_run: body.dry_run,
            ..ContextPut::default()
        })
    }

    /// Checks the rules a client can check: the messages
    /// ([`validate_messages`]), and a `queue` that names one of `tools`.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        validate_messages(&self.messages)?;
        crate::generate::validate_tools(&self.tools).map_err(|e| InvalidRequest(e.to_string()))?;
        if let Some(queue) = &self.queue
            && !self.tools.iter().any(|Tool::Function { function }| function.name == *queue)
        {
            return Err(InvalidRequest(format!("queue {queue:?} names no tool in tools")));
        }
        Ok(())
    }
}

/// Where a snapshot's state lives. A read from `host` or `disk` loads it
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Device,
    Host,
    Disk,
    /// A tier this client does not know.
    #[serde(other)]
    Unknown,
}

/// One held snapshot: the boundary at the end of message `message`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: SnapshotId,
    /// The index of the message whose end it holds, the system message
    /// counted.
    pub message: u64,
    pub tokens: u64,
    /// With the `tiers` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// The reply to `PUT /council/v2/contexts/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPutResult {
    pub id: ContextId,
    pub head: SnapshotId,
    pub tokens: u64,
    /// Tokens reused from `from`.
    pub kept: u64,
    /// Tokens this request ran, or would run under `dry_run`.
    pub fed: u64,
    /// The ancestor snapshot the build started from, held by this context
    /// or another, or `null`.
    #[serde(deserialize_with = "required_nullable")]
    pub from: Option<SnapshotId>,
    pub dry_run: bool,
    pub snapshots: Vec<SnapshotInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

/// The reply to `GET /council/v2/contexts/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextState {
    pub id: ContextId,
    pub head: SnapshotId,
    pub tokens: u64,
    pub snapshots: Vec<SnapshotInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
}

// ---- decisions

/// The thought before a decision's answer. `tokens: 0` closes it; above 0
/// needs the `think` capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Think {
    pub tokens: u64,
}

/// The body of `POST /council/v2/decisions`: v1's, plus `think`. The answer
/// is v1's [`v1::DecisionResponse`], each read with its `thought`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    #[serde(flatten)]
    pub decision: v1::DecisionRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<Think>,
}

impl DecisionRequest {
    /// v1's checks ([`v1::DecisionRequest::validate`]).
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.decision.validate()
    }
}

// ---- reads

/// The body of `POST /council/v2/reads`: each suffix read after each
/// context's head. Capability `reads`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadsRequest {
    pub contexts: Vec<ContextRef>,
    /// The client's own structure; special-token text tokenizes as single
    /// ids.
    pub suffixes: Vec<String>,
    pub letters: Vec<String>,
    /// How many of the most likely tokens each read reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<u32>,
}

impl ReadsRequest {
    /// Checks 1 to 8 distinct contexts, at least one suffix, and at least
    /// two distinct letters.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let bad = |m: &str| Err(InvalidRequest(m.to_string()));
        if self.contexts.is_empty() || self.contexts.len() > 8 {
            return bad("reads name 1 to 8 contexts");
        }
        for (i, c) in self.contexts.iter().enumerate() {
            if self.contexts[..i].iter().any(|p| p.id == c.id) {
                return Err(InvalidRequest(format!("context {} repeated", c.id)));
            }
        }
        if self.suffixes.is_empty() {
            return bad("reads need a suffix");
        }
        let letters = &self.letters;
        if letters.len() < 2 || letters.iter().enumerate().any(|(i, l)| l.is_empty() || letters[..i].contains(l)) {
            return bad("reads need 2 or more distinct, non-empty letters");
        }
        Ok(())
    }
}

/// A token and its log probability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TopToken {
    pub token: String,
    pub logprob: f64,
}

/// One context's read of one suffix.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawRead {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotId>,
    /// The index of the suffix read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<u64>,
    /// Each letter's full-vocabulary log probability.
    pub logprobs: IndexMap<String, f64>,
    /// `log Σ exp(logprobs)`.
    pub mass: f64,
    pub top: Vec<TopToken>,
}

/// The reply to `POST /council/v2/reads`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadsResponse {
    pub reads: Vec<RawRead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signals: Option<v1::Signals>,
}

// ---- generations

/// The body of `POST /council/v2/generations`, less `stream`, which the
/// calls set. Capability `generate`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub context: ContextId,
    /// Appended to the context before the reply, with boundaries where
    /// marked.
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
}

impl GenerationRequest {
    /// Checks the appended messages ([`validate_messages`]) and a positive
    /// `max_tokens`.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        validate_messages(&self.messages)?;
        if self.max_tokens == Some(0) {
            return Err(InvalidRequest("max_tokens must be at least 1".into()));
        }
        Ok(())
    }
}

/// A tool call the reply made. One that did not parse is kept as written,
/// never repaired and never dropped. The server names each call with the
/// `id` a `tool` message answers.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplyToolCall {
    Parsed { id: String, name: String, arguments: Json },
    Unparsed { id: String, name: Option<String>, raw: String, error: String },
}

impl ReplyToolCall {
    pub fn id(&self) -> &str {
        match self {
            ReplyToolCall::Parsed { id, .. } | ReplyToolCall::Unparsed { id, .. } => id,
        }
    }
}

impl<'de> Deserialize<'de> for ReplyToolCall {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        #[derive(Deserialize)]
        struct Raw {
            id: String,
            #[serde(default)]
            name: Option<String>,
            arguments: Option<Json>,
            raw: Option<String>,
            error: Option<String>,
        }
        let r = Raw::deserialize(de)?;
        if r.id.is_empty() {
            return Err(D::Error::custom("a reply tool call has an empty id"));
        }
        match (r.arguments, r.raw, r.error) {
            (Some(arguments @ Json::Object(_)), None, None) => match r.name {
                Some(name) if !name.is_empty() => Ok(ReplyToolCall::Parsed { id: r.id, name, arguments }),
                _ => Err(D::Error::custom("a parsed tool call has no name")),
            },
            (Some(_), None, None) => Err(D::Error::custom("tool call arguments are not an object")),
            (None, Some(raw), Some(error)) => Ok(ReplyToolCall::Unparsed { id: r.id, name: r.name, raw, error }),
            _ => Err(D::Error::custom("a tool call has either `arguments` or both `raw` and `error`")),
        }
    }
}

/// A generation's token counts: `kept` came from held state, `fed` was
/// prefilled now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct GenerationUsage {
    pub kept: u64,
    pub fed: u64,
    pub completion: u64,
}

/// The finished reply, which is now the context's head. It reports the
/// head and snapshots as a `PUT` reply does.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct GenerationDone {
    pub content: String,
    /// Empty when the model did not think.
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub tool_calls: Vec<ReplyToolCall>,
    pub finish: Finish,
    pub usage: GenerationUsage,
    pub head: SnapshotId,
    pub snapshots: Vec<SnapshotInfo>,
}

/// One event of a streamed generation.
#[derive(Clone, Debug, PartialEq)]
pub enum GenerationEvent {
    /// `/mk/v1`'s token event.
    Token(TokenEvent),
    /// Always the last event.
    Done(GenerationDone),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn snap(prefix: &str) -> String {
        format!("snap:{prefix}{}", "0".repeat(64 - prefix.len()))
    }

    const CTX: &str = "0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b12";

    /// The draft's `PUT` example, with its elisions filled in.
    fn doc_put() -> serde_json::Value {
        json!({"thinking": true,
               "tools": [{"type": "function", "function": {"name": "next_command", "description": "The next command.",
                          "parameters": {"type": "object", "properties": {"answers": {"type": "object"}}}}}],
               "queue": "next_command",
               "messages": [
                 {"role": "system", "content": "You are a policy expert for shell commands."},
                 {"role": "user", "content": "Commands from a coding agent wait in a queue.", "snap": true},
                 {"role": "assistant", "reasoning": "I should get the first command.",
                  "tool_calls": [{"id": "c0", "name": "next_command", "arguments": {}}]},
                 {"role": "tool", "tool_call_id": "c0", "content": "{\"command\": \"ls -la src\"}"},
                 {"role": "assistant", "reasoning": "ls only lists.",
                  "tool_calls": [{"id": "c1", "name": "next_command", "arguments": {"answers": {"verdict": "A"}}}],
                  "snap": true}]})
    }

    fn doc_put_result() -> serde_json::Value {
        json!({"id": CTX, "head": snap("51d0"), "tokens": 1840, "kept": 0, "fed": 1840,
               "from": null, "dry_run": false,
               "snapshots": [{"id": snap("9a1e"), "message": 1, "tokens": 412, "tier": "device"},
                             {"id": snap("51d0"), "message": 4, "tokens": 1840, "tier": "device"}]})
    }

    #[test]
    fn the_drafts_context_put_round_trips_and_validates() {
        let put: ContextPut = serde_json::from_value(doc_put()).unwrap();
        put.validate().unwrap();
        assert_eq!(put.messages.len(), 5);
        assert_eq!(put.messages[3].tool_call_id.as_deref(), Some("c0"));
        assert!(put.messages[1].snap && !put.messages[2].snap && put.messages[4].snap);
        assert_eq!(put.queue.as_deref(), Some("next_command"));
        assert_eq!(serde_json::to_value(&put).unwrap(), doc_put());
    }

    #[test]
    fn the_drafts_put_result_decodes_with_from_and_tiers() {
        let r: ContextPutResult = serde_json::from_value(doc_put_result()).unwrap();
        assert_eq!(r.from, None);
        assert_eq!(r.snapshots.iter().map(|s| (s.message, s.tier)).collect::<Vec<_>>(), [(1, Some(Tier::Device)), (4, Some(Tier::Device))]);
        assert_eq!(serde_json::to_value(&r).unwrap(), doc_put_result());
        let mut later = doc_put_result();
        later["from"] = json!(snap("9a1e"));
        later["snapshots"][0]["tier"] = json!("nvram");
        later["extra"] = json!(1);
        let r: ContextPutResult = serde_json::from_value(later).unwrap();
        assert_eq!(r.from.unwrap().as_str(), snap("9a1e"));
        assert_eq!(r.snapshots[0].tier, Some(Tier::Unknown), "a newer tier is not an error");
        for missing in ["from", "kept", "head"] {
            let mut v = doc_put_result();
            v.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<ContextPutResult>(v).is_err(), "{missing}");
        }
        let mut v = doc_put_result();
        v["snapshots"][0].as_object_mut().unwrap().remove("message");
        assert!(serde_json::from_value::<ContextPutResult>(v).is_err(), "a snapshot names its message");
    }

    #[test]
    fn a_fork_and_the_v1_options_serialize_only_when_set() {
        let put = ContextPut {
            messages: vec![Message::system("s"), Message::user("u").snapped()],
            fork: Some(Fork { context: CTX.into(), at: SnapshotId::parse(snap("9a1e")).unwrap() }),
            persist: Some(false),
            ..ContextPut::default()
        };
        assert_eq!(
            serde_json::to_value(&put).unwrap(),
            json!({"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "u", "snap": true}],
                   "fork": {"context": CTX, "at": snap("9a1e")}, "persist": false})
        );
    }

    #[test]
    fn messages_the_draft_refuses_are_refused() {
        let call = |id: &str, args: Json| ToolCall { id: id.into(), name: "ls".into(), arguments: args };
        let obj = || Json::Object(Default::default());
        let cases: Vec<(&str, Vec<Message>)> = vec![
            ("reasoning on user", vec![Message { reasoning: Some("r".into()), ..Message::user("u") }]),
            ("calls on user", vec![Message { tool_calls: vec![call("a", obj())], ..Message::user("u") }]),
            ("tool without id", vec![Message { tool_call_id: None, ..Message::tool("a", "r") }]),
            ("id on user", vec![Message { tool_call_id: Some("a".into()), ..Message::user("u") }]),
            ("empty call id", vec![Message { tool_calls: vec![call("", obj())], ..Message::assistant("") }]),
            ("string arguments", vec![Message { tool_calls: vec![call("a", Json::from("x"))], ..Message::assistant("") }]),
        ];
        for (name, messages) in cases {
            assert!(validate_messages(&messages).is_err(), "{name}");
        }
        let mut queue: ContextPut = serde_json::from_value(doc_put()).unwrap();
        queue.queue = Some("missing".into());
        assert!(queue.validate().is_err(), "queue names a tool");
    }

    /// A system message after the first is system text at that point, in a
    /// whole context and in the messages a generation appends. It still
    /// carries no reasoning or calls.
    #[test]
    fn a_later_system_message_is_accepted() {
        let nudge = "You replied without a tool call, so the task is still open.";
        let context = [Message::system("s"), Message::user("u"), Message::assistant("a"), Message::system(nudge)];
        validate_messages(&context).unwrap();
        validate_messages(&[Message::system(nudge)]).unwrap();
        let generation = GenerationRequest { context: CTX.into(), messages: vec![Message::system(nudge)], max_tokens: None, sample: None };
        generation.validate().unwrap();
        assert!(validate_messages(&[Message { reasoning: Some("r".into()), ..Message::system(nudge) }]).is_err());
    }

    #[test]
    fn a_v1_body_becomes_messages_with_the_system_first() {
        let v1_body = v1::ContextPut {
            system: "rules".into(),
            turns: vec![
                v1::Turn { role: v1::Role::User, content: "a".into(), snap: false, reasoning: None },
                v1::Turn { role: v1::Role::Assistant, content: "b".into(), snap: true, reasoning: Some("why".into()) },
            ],
            pin: None,
            warm: None,
            dry_run: None,
            persist: Some(false),
        };
        let put = ContextPut::from_v1(v1_body.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&put).unwrap(),
            json!({"messages": [{"role": "system", "content": "rules"},
                                {"role": "user", "content": "a"},
                                {"role": "assistant", "content": "b", "reasoning": "why", "snap": true}],
                   "persist": false})
        );
        let tool = v1::ContextPut {
            turns: vec![v1::Turn { role: v1::Role::Tool, content: "x".into(), snap: false, reasoning: None }],
            ..v1_body
        };
        assert!(ContextPut::from_v1(tool).is_err());
    }

    #[test]
    fn a_decision_is_v1_plus_think() {
        let text = r#"{"state":"git push","spec_id":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","contexts":[{"id":"c"}],"think":{"tokens":64}}"#;
        let req: DecisionRequest = serde_json::from_str(text).unwrap();
        req.validate().unwrap();
        assert_eq!(req.think, Some(Think { tokens: 64 }));
        assert_eq!(req.decision.contexts.as_ref().unwrap()[0].id, "c");
        assert_eq!(serde_json::to_string(&req).unwrap(), text);
        let plain = DecisionRequest { think: None, ..req };
        assert!(serde_json::to_value(&plain).unwrap().get("think").is_none());
    }

    #[test]
    fn the_drafts_reads_request_round_trips_and_validates() {
        let doc = json!({"contexts": [{"id": CTX}],
                         "suffixes": ["<channel|><|tool_call>call:next_command{answers:{verdict:<|\"|>"],
                         "letters": ["A", "B", "C"], "top": 5});
        let req: ReadsRequest = serde_json::from_value(doc.clone()).unwrap();
        req.validate().unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), doc);
        for bad in [
            ReadsRequest { contexts: vec![], ..req.clone() },
            ReadsRequest { suffixes: vec![], ..req.clone() },
            ReadsRequest { letters: vec!["A".into(), "A".into()], ..req.clone() },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        let reply: ReadsResponse = serde_json::from_value(json!({"reads": [
            {"context": CTX, "snapshot": snap("51d0"), "suffix": 0,
             "logprobs": {"A": -0.01, "B": -5.0, "C": -6.0}, "mass": -0.003,
             "top": [{"token": "A", "logprob": -0.01}]}]}))
        .unwrap();
        assert_eq!(reply.reads[0].logprobs.keys().collect::<Vec<_>>(), ["A", "B", "C"]);
    }

    #[test]
    fn the_drafts_generation_request_round_trips() {
        let doc = json!({"context": CTX, "messages": [{"role": "user", "content": "how did it go?", "snap": true}],
                         "max_tokens": 2048, "sample": {"temperature": 1.0}});
        let req: GenerationRequest = serde_json::from_value(doc.clone()).unwrap();
        req.validate().unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), doc);
        assert!(GenerationRequest { max_tokens: Some(0), ..req }.validate().is_err());
    }

    fn done() -> serde_json::Value {
        json!({"content": "", "reasoning": "list it",
               "tool_calls": [{"id": "c2", "name": "ls", "arguments": {"path": "/tmp", "all": true}}],
               "finish": "stop", "usage": {"kept": 1840, "fed": 12, "completion": 30},
               "head": snap("77aa"), "snapshots": [{"id": snap("9a1e"), "message": 1, "tokens": 412}]})
    }

    #[test]
    fn a_generations_done_keeps_call_ids_and_argument_order() {
        let mut text = done().to_string();
        text = text.replace(r#"{"all":true,"path":"/tmp"}"#, r#"{"path":"/tmp","all":true}"#);
        let d: GenerationDone = serde_json::from_str(&text).unwrap();
        let [ReplyToolCall::Parsed { id, name, arguments }] = d.tool_calls.as_slice() else { panic!("{:?}", d.tool_calls) };
        assert_eq!((id.as_str(), name.as_str()), ("c2", "ls"));
        assert_eq!(serde_json::to_string(arguments).unwrap(), r#"{"path":"/tmp","all":true}"#);
        assert_eq!((d.usage.kept, d.usage.fed, d.head.as_str()), (1840, 12, snap("77aa").as_str()));
        let mut unparsed = done();
        unparsed["tool_calls"] = json!([{"id": "c3", "name": null, "raw": "<function=", "error": "no name"}]);
        let d: GenerationDone = serde_json::from_value(unparsed).unwrap();
        assert_eq!(d.tool_calls[0], ReplyToolCall::Unparsed { id: "c3".into(), name: None, raw: "<function=".into(), error: "no name".into() });
        for bad in [
            json!({"name": "ls", "arguments": {}}),
            json!({"id": "", "name": "ls", "arguments": {}}),
            json!({"id": "c", "name": "ls", "arguments": "x"}),
            json!({"id": "c", "arguments": {}}),
            json!({"id": "c", "name": "ls", "raw": "x"}),
        ] {
            let mut v = done();
            v["tool_calls"] = json!([bad]);
            assert!(serde_json::from_value::<GenerationDone>(v).is_err(), "{bad}");
        }
        for missing in ["head", "snapshots", "usage", "finish", "content"] {
            let mut v = done();
            v.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<GenerationDone>(v).is_err(), "{missing}");
        }
    }
}
