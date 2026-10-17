//! Request and response types council v2 adds or changes
//! (`docs/council-api-v2.md`).
//!
//! Requests serialize exactly the draft's fields and leave out absent
//! optionals. Responses are strict about required fields and types and
//! tolerate fields the draft does not name, as v1's are. Snapshot ids keep
//! v1's form, `snap:` and 64 lowercase hex digits.
//!
//! Every request that reads a context carries it whole as a [`Context`];
//! the server keeps no context records. Where the draft is silent, these
//! types choose, and each choice is named on its type: a reply tool call
//! carries the `id` the server gave it, a generation's `token` events are
//! `/mk/v1`'s, a `reads` reply is `reads` with each read's `logprobs`,
//! `mass`, and `top`, and a pin set's `GET` lists its snapshot ids.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

use crate::council::wire::{self as v1, ContextRef, InvalidRequest, SnapshotId};
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

/// A chat context, sent whole with every request that reads it. It is a
/// value, not a record: the client keeps it, and the server starts each
/// request from the longest snapshot it holds along it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// Absent: the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// A tool in `tools` whose call receives a decision's case as its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    pub messages: Vec<Message>,
}

impl Context {
    /// v1's body as a v2 context: the system message first, then each turn
    /// as a message with its `snap` and `reasoning`. v1's `pin`, `persist`,
    /// `warm`, and `dry_run` have no place in a context and are dropped. A
    /// v1 `tool` turn names no call, so it has no v2 form and is refused.
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
        Ok(Context { messages, ..Context::default() })
    }

    /// Checks the rules a client can check: the messages
    /// ([`validate_messages`]), the tools, and a `queue` that names one of
    /// `tools`.
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

/// The body of `POST /council/v2/contexts`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextBuild {
    pub context: Context,
    /// Renders and reports `kept`, `fed`, and `from` without feeding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
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

/// One held snapshot of a context: the boundary at the end of message
/// `message`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: SnapshotId,
    /// The index of the message whose end it holds, the system message
    /// counted.
    pub message: u64,
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// The reply to `POST /council/v2/contexts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBuilt {
    /// The snapshot at the context's end.
    pub head: SnapshotId,
    pub tokens: u64,
    /// Tokens reused from `from`.
    pub kept: u64,
    /// Tokens this request ran, or would run under `dry_run`.
    pub fed: u64,
    /// The snapshot the build started from, held from this context or any
    /// other, or `null`.
    #[serde(deserialize_with = "required_nullable")]
    pub from: Option<SnapshotId>,
    pub dry_run: bool,
    pub snapshots: Vec<SnapshotInfo>,
}

// ---- snapshots and pins

/// The longest pin a `PUT /council/v2/pins/{owner}` may ask for: a day.
pub const PIN_TTL_MAX_S: u64 = 86_400;

/// Refuses an owner the draft refuses: empty, over 128 characters, not
/// printable, or holding a slash.
pub fn validate_owner(owner: &str) -> Result<(), InvalidRequest> {
    if owner.is_empty() || owner.chars().count() > 128 || owner.contains('/') || owner.chars().any(char::is_control) {
        return Err(InvalidRequest(format!(
            "pin owner {owner:?} is not 1 to 128 printable characters without a slash"
        )));
    }
    Ok(())
}

/// An owner of a pin on a snapshot, and the seconds its pins have left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinHolder {
    pub owner: String,
    pub expires_in_s: u64,
}

/// The reply to `GET /council/v2/snapshots/{id}`. `pins` names the owners
/// that pin this snapshot, not the ones whose pins keep it as part of a
/// chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotState {
    pub id: SnapshotId,
    pub tokens: u64,
    pub tier: Tier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub pins: Vec<PinHolder>,
}

/// The body of `PUT /council/v2/pins/{owner}`: the owner's whole set, held
/// for `ttl_s` seconds from now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinsPut {
    pub snapshots: Vec<SnapshotId>,
    pub ttl_s: u64,
}

impl PinsPut {
    /// Checks a `ttl_s` of 1 to [`PIN_TTL_MAX_S`].
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if self.ttl_s == 0 || self.ttl_s > PIN_TTL_MAX_S {
            return Err(InvalidRequest(format!("ttl_s {} is not 1 to {PIN_TTL_MAX_S}", self.ttl_s)));
        }
        Ok(())
    }
}

/// One snapshot of a `PUT` set. `held: false` holds nothing: the server
/// does not hold that snapshot, and the client builds its context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinHeld {
    pub id: SnapshotId,
    pub held: bool,
}

/// The reply to `PUT /council/v2/pins/{owner}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinsPutResult {
    pub owner: String,
    pub snapshots: Vec<PinHeld>,
    /// `null` for an empty set, which holds nothing.
    #[serde(deserialize_with = "required_nullable")]
    pub expires_in_s: Option<u64>,
}

/// The reply to `GET /council/v2/pins/{owner}`. The draft does not give its
/// `snapshots` a shape: each entry is read as a snapshot id or as an object
/// with an `id`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PinSet {
    pub owner: String,
    #[serde(deserialize_with = "pinned_ids")]
    pub snapshots: Vec<SnapshotId>,
    pub expires_in_s: u64,
}

fn pinned_ids<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<SnapshotId>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Id(SnapshotId),
        Object { id: SnapshotId },
    }
    Ok(Vec::<Entry>::deserialize(de)?
        .into_iter()
        .map(|e| match e {
            Entry::Id(id) | Entry::Object { id } => id,
        })
        .collect())
}

// ---- decisions

/// The thought before a decision's answer. `tokens: 0` closes it; above 0
/// needs the `think` capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Think {
    pub tokens: u64,
}

/// The body of `POST /council/v2/decisions`: v1's without its `contexts`,
/// the contexts sent whole, and `think`. The answer is v1's
/// [`v1::DecisionResponse`]; each read names its context by its index in
/// `contexts`, as do the keys of `leave_one_out`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    /// Its `contexts` stays `None`.
    #[serde(flatten)]
    pub decision: v1::DecisionRequest,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<Context>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<Think>,
}

impl DecisionRequest {
    /// The request as v1 would name its contexts: each by its index in
    /// `contexts`, which is how a v2 answer names them. Math checks
    /// ([`crate::council::math::verify`]) read this view.
    pub fn by_index(&self) -> v1::DecisionRequest {
        let names = (!self.contexts.is_empty())
            .then(|| (0..self.contexts.len()).map(|i| ContextRef { id: i.to_string(), at: None }).collect());
        v1::DecisionRequest { contexts: names, ..self.decision.clone() }
    }

    /// v1's checks over [`Self::by_index`], each context's own
    /// ([`Context::validate`]), and no v1 `contexts`.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if self.decision.contexts.is_some() {
            return Err(InvalidRequest("a council v2 decision sends its contexts whole, not v1 context ids".into()));
        }
        self.by_index().validate()?;
        for (i, c) in self.contexts.iter().enumerate() {
            c.validate().map_err(|e| InvalidRequest(format!("context {i}: {}", e.0)))?;
        }
        Ok(())
    }

    /// Recomputes `response`'s derived numbers from its raw reads
    /// ([`crate::council::math::verify`]), its reads named by index.
    pub fn verify(&self, response: &v1::DecisionResponse) -> Result<(), crate::council::math::Mismatch> {
        crate::council::math::verify(response, &self.by_index())
    }
}

// ---- reads

/// The body of `POST /council/v2/reads`: each suffix read after each
/// context's end. Capability `reads`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadsRequest {
    pub contexts: Vec<Context>,
    /// The client's own structure; special-token text tokenizes as single
    /// ids.
    pub suffixes: Vec<String>,
    pub letters: Vec<String>,
    /// How many of the most likely tokens each read reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<u32>,
}

impl ReadsRequest {
    /// Checks 1 to 8 valid contexts, at least one suffix, and at least two
    /// distinct letters.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let bad = |m: &str| Err(InvalidRequest(m.to_string()));
        if self.contexts.is_empty() || self.contexts.len() > 8 {
            return bad("reads send 1 to 8 contexts");
        }
        for (i, c) in self.contexts.iter().enumerate() {
            c.validate().map_err(|e| InvalidRequest(format!("context {i}: {}", e.0)))?;
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
    /// The index of the context read, in `contexts`.
    pub context: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotId>,
    /// The index of the suffix read.
    pub suffix: u64,
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
/// calls set. The reply follows the context's end. Capability `generate`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub context: Context,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
}

impl GenerationRequest {
    /// Checks the context ([`Context::validate`]) and a positive
    /// `max_tokens`.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.context.validate()?;
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
/// prefilled now, and the two make `prompt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct GenerationUsage {
    pub prompt: u64,
    pub kept: u64,
    pub fed: u64,
    pub completion: u64,
}

/// The finished reply. The client appends it to its own messages for the
/// next request.
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
    /// The snapshot after the reply when the server holds it, else `null`,
    /// and the next request starts from the context's last boundary.
    #[serde(deserialize_with = "required_nullable")]
    pub head: Option<SnapshotId>,
    /// The context's snapshots.
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

    /// The draft's context example, with its elisions filled in.
    fn doc_context() -> serde_json::Value {
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

    fn doc_built() -> serde_json::Value {
        json!({"head": snap("51d0"), "tokens": 1840, "kept": 0, "fed": 1840,
               "from": null, "dry_run": false,
               "snapshots": [{"id": snap("9a1e"), "message": 1, "tokens": 412, "tier": "host"},
                             {"id": snap("51d0"), "message": 4, "tokens": 1840, "tier": "host"}]})
    }

    #[test]
    fn the_drafts_context_round_trips_and_validates() {
        let build = json!({"context": doc_context()});
        let b: ContextBuild = serde_json::from_value(build.clone()).unwrap();
        b.context.validate().unwrap();
        assert_eq!(b.context.messages.len(), 5);
        assert_eq!(b.context.messages[3].tool_call_id.as_deref(), Some("c0"));
        assert!(b.context.messages[1].snap && !b.context.messages[2].snap && b.context.messages[4].snap);
        assert_eq!(b.context.queue.as_deref(), Some("next_command"));
        assert_eq!(serde_json::to_value(&b).unwrap(), build);
        let dry = ContextBuild { dry_run: Some(true), ..b };
        assert_eq!(serde_json::to_value(&dry).unwrap()["dry_run"], json!(true));
    }

    #[test]
    fn the_drafts_build_reply_decodes_with_from_and_tiers() {
        let r: ContextBuilt = serde_json::from_value(doc_built()).unwrap();
        assert_eq!(r.from, None);
        assert_eq!(r.snapshots.iter().map(|s| (s.message, s.tier)).collect::<Vec<_>>(), [(1, Some(Tier::Host)), (4, Some(Tier::Host))]);
        assert_eq!(serde_json::to_value(&r).unwrap(), doc_built());
        let mut later = doc_built();
        later["from"] = json!(snap("9a1e"));
        later["snapshots"][0]["tier"] = json!("nvram");
        later["extra"] = json!(1);
        let r: ContextBuilt = serde_json::from_value(later).unwrap();
        assert_eq!(r.from.unwrap().as_str(), snap("9a1e"));
        assert_eq!(r.snapshots[0].tier, Some(Tier::Unknown), "a newer tier is not an error");
        for missing in ["from", "kept", "head"] {
            let mut v = doc_built();
            v.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<ContextBuilt>(v).is_err(), "{missing}");
        }
        let mut v = doc_built();
        v["snapshots"][0].as_object_mut().unwrap().remove("message");
        assert!(serde_json::from_value::<ContextBuilt>(v).is_err(), "a snapshot names its message");
    }

    #[test]
    fn the_drafts_snapshot_and_pins_examples_decode() {
        let state: SnapshotState = serde_json::from_value(json!({"id": snap("51d0"), "tokens": 1840, "tier": "host",
            "bytes": 829030400u64, "pins": [{"owner": "kernel-7f3a", "expires_in_s": 3412}]}))
        .unwrap();
        assert_eq!((state.tier, state.pins[0].owner.as_str(), state.pins[0].expires_in_s), (Tier::Host, "kernel-7f3a", 3412));
        let put = PinsPut { snapshots: vec![SnapshotId::parse(snap("51d0")).unwrap()], ttl_s: 3600 };
        put.validate().unwrap();
        assert_eq!(serde_json::to_value(&put).unwrap(), json!({"snapshots": [snap("51d0")], "ttl_s": 3600}));
        for ttl_s in [0, PIN_TTL_MAX_S + 1] {
            assert!(PinsPut { ttl_s, ..put.clone() }.validate().is_err(), "{ttl_s}");
        }
        let r: PinsPutResult = serde_json::from_value(json!({"owner": "kernel-7f3a",
            "snapshots": [{"id": snap("51d0"), "held": true}, {"id": snap("c4e2"), "held": false}], "expires_in_s": 3600}))
        .unwrap();
        assert_eq!(r.snapshots.iter().map(|s| s.held).collect::<Vec<_>>(), [true, false]);
        let empty: PinsPutResult = serde_json::from_value(json!({"owner": "k", "snapshots": [], "expires_in_s": null})).unwrap();
        assert_eq!(empty.expires_in_s, None);
        for snapshots in [json!([snap("51d0")]), json!([{"id": snap("51d0"), "held": true}])] {
            let set: PinSet = serde_json::from_value(json!({"owner": "k", "snapshots": snapshots, "expires_in_s": 10})).unwrap();
            assert_eq!(set.snapshots, [SnapshotId::parse(snap("51d0")).unwrap()]);
        }
        validate_owner("kernel-7f3a").unwrap();
        for bad in ["", "a/b", "tab\there", &"k".repeat(129)] {
            assert!(validate_owner(bad).is_err(), "{bad:?}");
        }
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
        let mut queue: Context = serde_json::from_value(doc_context()).unwrap();
        queue.queue = Some("missing".into());
        assert!(queue.validate().is_err(), "queue names a tool");
    }

    /// A system message after the first is system text at that point. It
    /// still carries no reasoning or calls.
    #[test]
    fn a_later_system_message_is_accepted() {
        let nudge = "You replied without a tool call, so the task is still open.";
        let context = [Message::system("s"), Message::user("u"), Message::assistant("a"), Message::system(nudge)];
        validate_messages(&context).unwrap();
        let generation = GenerationRequest {
            context: Context { messages: context.to_vec(), ..Context::default() },
            max_tokens: None,
            sample: None,
        };
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
            pin: Some(true),
            warm: None,
            dry_run: None,
            persist: Some(false),
        };
        let context = Context::from_v1(v1_body.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&context).unwrap(),
            json!({"messages": [{"role": "system", "content": "rules"},
                                {"role": "user", "content": "a"},
                                {"role": "assistant", "content": "b", "reasoning": "why", "snap": true}]})
        );
        let tool = v1::ContextPut {
            turns: vec![v1::Turn { role: v1::Role::Tool, content: "x".into(), snap: false, reasoning: None }],
            ..v1_body
        };
        assert!(Context::from_v1(tool).is_err());
    }

    /// A v2 decision sends its contexts whole and names them by index, so
    /// its math view names each read's context as the server does.
    ///
    /// Falsified by a request that keeps v1 ids or names contexts any other
    /// way.
    #[test]
    fn a_decision_sends_its_contexts_whole_and_names_them_by_index() {
        let spec = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let text = json!({"state": "git push", "spec_id": spec, "contexts": [doc_context(), doc_context()], "think": {"tokens": 64}});
        let req: DecisionRequest = serde_json::from_value(text.clone()).unwrap();
        req.validate().unwrap();
        assert_eq!(req.think, Some(Think { tokens: 64 }));
        assert_eq!(req.contexts.len(), 2);
        assert_eq!(req.decision.contexts, None);
        assert_eq!(serde_json::to_value(&req).unwrap(), text);
        let ids: Vec<String> = req.by_index().contexts.unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["0", "1"]);
        let plain = DecisionRequest { think: None, ..req.clone() };
        assert!(serde_json::to_value(&plain).unwrap().get("think").is_none());
        let v1_ids = DecisionRequest {
            decision: v1::DecisionRequest { contexts: Some(vec![ContextRef { id: "c".into(), at: None }]), ..req.decision.clone() },
            ..req
        };
        assert!(v1_ids.validate().is_err(), "a v1 context id is refused");
    }

    #[test]
    fn the_drafts_reads_request_round_trips_and_validates() {
        let doc = json!({"contexts": [doc_context()],
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
            {"context": 0, "snapshot": snap("51d0"), "suffix": 0,
             "logprobs": {"A": -0.01, "B": -5.0, "C": -6.0}, "mass": -0.003,
             "top": [{"token": "A", "logprob": -0.01}]}]}))
        .unwrap();
        assert_eq!(reply.reads[0].context, 0);
        assert_eq!(reply.reads[0].logprobs.keys().collect::<Vec<_>>(), ["A", "B", "C"]);
    }

    #[test]
    fn the_drafts_generation_request_round_trips() {
        let mut context = doc_context();
        context["messages"].as_array_mut().unwrap().push(json!({"role": "user", "content": "how did it go?", "snap": true}));
        let doc = json!({"context": context, "max_tokens": 2048, "sample": {"temperature": 1.0}});
        let req: GenerationRequest = serde_json::from_value(doc.clone()).unwrap();
        req.validate().unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), doc);
        assert!(GenerationRequest { max_tokens: Some(0), ..req }.validate().is_err());
    }

    fn done() -> serde_json::Value {
        json!({"content": "", "reasoning": "list it",
               "tool_calls": [{"id": "c2", "name": "ls", "arguments": {"path": "/tmp", "all": true}}],
               "finish": "stop", "usage": {"prompt": 1852, "kept": 1840, "fed": 12, "completion": 30},
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
        assert_eq!((d.usage.prompt, d.usage.kept, d.usage.fed), (1852, 1840, 12));
        assert_eq!(d.head.as_ref().map(|h| h.as_str()), Some(snap("77aa").as_str()));
        let mut unheld = done();
        unheld["head"] = json!(null);
        assert_eq!(serde_json::from_value::<GenerationDone>(unheld).unwrap().head, None, "a head the server did not hold");
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
