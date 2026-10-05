//! Request and response types of the council API, as `docs/council-api.openapi.yaml`
//! defines them.
//!
//! Requests serialize exactly the contract's fields and leave out absent
//! optionals. Responses are strict about required fields and types and
//! tolerate fields the contract does not name. Maps that carry an order
//! (options, levels, questions) are [`IndexMap`]s, so the order on the wire is
//! the order here.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

use crate::json::Json;

/// A context id: a UUID the client chose.
pub type ContextId = String;
/// A question id: the client's name for a question.
pub type QuestionId = String;

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

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

macro_rules! prefixed_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Checks the form `<prefix><64 lowercase hex digits>`.
            pub fn parse(s: impl Into<String>) -> Result<Self, String> {
                Self::try_from(s.into())
            }

            /// The id as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(s: String) -> Result<Self, String> {
                match s.strip_prefix($prefix) {
                    Some(rest) if is_lower_hex(rest, 64) => Ok(Self(s)),
                    _ => Err(format!(concat!("not a ", stringify!($name), ": {:?}"), s)),
                }
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

prefixed_id!(
    /// `snap:` and 64 hex digits: the opaque address of one build of a context prefix.
    SnapshotId,
    "snap:"
);
prefixed_id!(
    /// `sha256:` and 64 hex digits: the hash of a spec's canonical JSON.
    SpecId,
    "sha256:"
);

// ---- contexts

/// Who spoke a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// One turn of a held context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub content: String,
    /// Keep a snapshot boundary at the end of this turn. Left out when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub snap: bool,
    /// The model's own thinking on an assistant turn, which the server renders
    /// as that turn's thinking region. Left out when absent; any other role
    /// carrying it is a `400`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

/// The body of `PUT /council/v1/contexts/{id}`: the whole context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPut {
    pub system: String,
    pub turns: Vec<Turn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm: Option<Vec<SpecId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// `false` says the client will `PUT` this context again after a restart
    /// or an eviction, so the server need not keep it beyond memory. Absent
    /// keeps the server's setting. Requires the `persist` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
}

/// Which layer of the stack a snapshot ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    System,
    Turn,
    Spec,
}

/// One held snapshot of a context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: SnapshotId,
    pub tokens: u64,
    pub layer: Layer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_id: Option<SpecId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

/// The reply to `GET /council/v1/contexts/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextState {
    pub id: ContextId,
    pub head: SnapshotId,
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
    pub snapshots: Vec<SnapshotInfo>,
}

/// The reply to `PUT /council/v1/contexts/{id}`: the state plus what the build cost.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPutResult {
    pub id: ContextId,
    pub head: SnapshotId,
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    pub snapshots: Vec<SnapshotInfo>,
    /// Tokens reused from a held build.
    pub kept: u64,
    /// Tokens this request ran, or would run under `dry_run`.
    pub fed: u64,
    /// True when nothing was built and `head` is the id the build would have.
    pub dry_run: bool,
}

// ---- specs

/// One option of a spec `choice` question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub option: String,
    pub means: String,
}

/// A spec `choice` question. Options are ordered; ties go to the earlier one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpecChoice {
    pub id: QuestionId,
    pub instructions: Json,
    pub criteria: Vec<ChoiceOption>,
}

/// A spec `score` question. Levels are named lowest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpecScore {
    pub id: QuestionId,
    pub instructions: Json,
    pub criteria: Vec<String>,
}

/// A spec `noul` (yes or no) question. An explicit `null` criteria reads back as absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpecNoul {
    pub id: QuestionId,
    pub instructions: Json,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Json>,
}

/// A spec `text` question: the model writes the answer, up to `max_tokens`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpecText {
    pub id: QuestionId,
    pub instructions: Json,
    pub max_tokens: u64,
}

/// A question of a spec, tagged by `type` on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SpecQuestion {
    Choice(SpecChoice),
    Score(SpecScore),
    Noul(SpecNoul),
    Text(SpecText),
}

impl SpecQuestion {
    /// The question's id.
    pub fn id(&self) -> &str {
        match self {
            SpecQuestion::Choice(q) => &q.id,
            SpecQuestion::Score(q) => &q.id,
            SpecQuestion::Noul(q) => &q.id,
            SpecQuestion::Text(q) => &q.id,
        }
    }

    /// Replaces the question's instructions.
    pub fn set_instructions(&mut self, instructions: Json) {
        match self {
            SpecQuestion::Choice(q) => q.instructions = instructions,
            SpecQuestion::Score(q) => q.instructions = instructions,
            SpecQuestion::Noul(q) => q.instructions = instructions,
            SpecQuestion::Text(q) => q.instructions = instructions,
        }
    }
}

/// A held question set. Its id is the hash of its canonical JSON
/// ([`crate::canon::spec_id`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub name: String,
    pub instructions: String,
    pub input_label: String,
    pub questions: Vec<SpecQuestion>,
}

/// The reply to `POST /council/v1/specs` and `GET /council/v1/specs/{id}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeldSpec {
    pub spec_id: SpecId,
    pub spec: Spec,
    /// How the server compiles and renders the spec; part of its identity.
    pub template: String,
}

/// An inline Decisions `choice` question; options read in the order listed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InlineChoice {
    pub instructions: Json,
    pub criteria: IndexMap<String, String>,
}

/// An inline Decisions `score` question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InlineScore {
    pub instructions: Json,
    pub criteria: Vec<String>,
}

/// An inline Decisions `noul` question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InlineNoul {
    pub instructions: Json,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Json>,
}

/// An inline Decisions question, tagged by `type`. There is no `text` kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InlineQuestion {
    Choice(InlineChoice),
    Score(InlineScore),
    Noul(InlineNoul),
}

// ---- decisions

/// A context named in a decision, optionally pinned to a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRef {
    pub id: ContextId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<SnapshotId>,
}

/// How reads are pooled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolMethod {
    /// The weighted mean of probabilities.
    Linear,
    /// The normalized weighted product.
    Loglinear,
}

/// How reads are weighted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolWeights {
    Uniform,
    /// Each read's `exp(mass)` for that question.
    Mass,
    /// The request's `values`.
    Given,
}

/// Pool settings in a request. Absent fields take the server's defaults:
/// `linear` and `uniform`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<PoolMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<PoolWeights>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<f64>>,
}

/// The body of `POST /council/v1/decisions`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A string, object, or array; objects keep member order.
    pub state: Json,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_id: Option<SpecId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<Vec<QuestionId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<IndexMap<QuestionId, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<IndexMap<QuestionId, InlineQuestion>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<Vec<ContextRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<Pool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Json>,
}

/// A request the client refuses to send because the contract forbids it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid decision request: {0}")]
pub struct InvalidRequest(pub String);

impl DecisionRequest {
    /// Checks the rules a client can check without the server: exactly one of
    /// `spec_id` and `questions`; `ask` and `options` only with a spec; a
    /// string, object, or array `state`; at most 8 distinct contexts; a
    /// positive `timeout_ms`; and pool `values` that go with `given` weights,
    /// one per context, finite, at least 0, summing above 0.
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let bad = |m: &str| Err(InvalidRequest(m.to_string()));
        match (&self.spec_id, &self.questions) {
            (Some(_), Some(_)) => return bad("both spec_id and questions"),
            (None, None) => return bad("neither spec_id nor questions"),
            (None, Some(q)) => {
                if q.is_empty() {
                    return bad("questions is empty");
                }
                if self.ask.is_some() || self.options.is_some() {
                    return bad("ask and options go with spec_id, not questions");
                }
            }
            (Some(_), None) => {}
        }
        if !matches!(self.state, Json::String(_) | Json::Object(_) | Json::Array(_)) {
            return bad("state must be a string, object, or array");
        }
        let contexts = self.contexts.as_deref().unwrap_or(&[]);
        if contexts.len() > 8 {
            return bad("more than 8 contexts");
        }
        for (i, c) in contexts.iter().enumerate() {
            if contexts[..i].iter().any(|p| p.id == c.id) {
                return Err(InvalidRequest(format!("context {} repeated", c.id)));
            }
        }
        if self.timeout_ms == Some(0) {
            return bad("timeout_ms must be at least 1");
        }
        if let Some(opts) = &self.options {
            for (q, o) in opts {
                if o.len() < 2 || o.iter().enumerate().any(|(i, x)| o[..i].contains(x)) {
                    return Err(InvalidRequest(format!("options for {q} need 2 or more distinct names")));
                }
            }
        }
        let pool = self.pool.clone().unwrap_or_default();
        match (pool.weights, &pool.values) {
            (Some(PoolWeights::Given), Some(v)) => {
                if v.len() != contexts.len().max(1) {
                    return bad("pool values need one entry per read");
                }
                if v.iter().any(|x| !x.is_finite() || *x < 0.0) || v.iter().sum::<f64>().partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                    return bad("pool values must be finite, at least 0, and sum above 0");
                }
            }
            (Some(PoolWeights::Given), None) => return bad("weights given needs values"),
            (_, Some(_)) => return bad("pool values go only with weights given"),
            _ => {}
        }
        Ok(())
    }
}

// ---- answers

/// A `choice` answer from one read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadChoice {
    pub choice: String,
    pub probabilities: IndexMap<String, f64>,
    pub confidence: f64,
    pub logprobs: IndexMap<String, f64>,
    pub mass: f64,
}

/// A `score` answer from one read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadScore {
    pub score: f64,
    pub legend: IndexMap<String, String>,
    pub probabilities: IndexMap<String, f64>,
    pub confidence: f64,
    pub logprobs: IndexMap<String, f64>,
    pub mass: f64,
}

/// A `noul` answer from one read; `logprobs` is keyed `yes` and `no`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadNoul {
    pub noul: f64,
    pub logprobs: IndexMap<String, f64>,
    pub mass: f64,
}

/// One context's answer to one question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ReadAnswer {
    Choice(ReadChoice),
    Score(ReadScore),
    Noul(ReadNoul),
}

/// A pooled `choice` answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PooledChoice {
    pub choice: String,
    pub probabilities: IndexMap<String, f64>,
    pub confidence: f64,
    pub agree: bool,
    pub spread: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leave_one_out: Option<IndexMap<ContextId, Option<IndexMap<String, f64>>>>,
}

/// A pooled `score` answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PooledScore {
    pub score: f64,
    pub legend: IndexMap<String, String>,
    pub probabilities: IndexMap<String, f64>,
    pub confidence: f64,
    pub agree: bool,
    pub spread: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leave_one_out: Option<IndexMap<ContextId, Option<IndexMap<String, f64>>>>,
}

/// A pooled `noul` answer: the pooled probability of yes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PooledNoul {
    pub noul: f64,
    pub agree: bool,
    pub spread: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leave_one_out: Option<IndexMap<ContextId, Option<f64>>>,
}

/// An answer pooled over the reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PooledAnswer {
    Choice(PooledChoice),
    Score(PooledScore),
    Noul(PooledNoul),
}

/// One context's read of the case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Read {
    /// The context read, or null for the spec alone.
    #[serde(deserialize_with = "required_nullable")]
    pub context: Option<ContextId>,
    /// The snapshot the read started from, or null for inline questions with no context.
    #[serde(deserialize_with = "required_nullable")]
    pub snapshot: Option<SnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub described: Option<IndexMap<QuestionId, String>>,
    pub answers: IndexMap<QuestionId, ReadAnswer>,
    /// sha256 of the rendered text this read was conditioned on.
    pub rendered_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
}

/// A control token that some text spelled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlTextHit {
    pub r#where: String,
    pub token: String,
}

/// Signals the server reports beside the answers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signals {
    #[serde(default)]
    pub control_text: Vec<ControlTextHit>,
}

/// The pool settings the server used.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PoolEcho {
    pub method: PoolMethod,
    pub weights: PoolWeights,
    /// By question id, the weight each read pooled with, in read order.
    pub normalized: IndexMap<QuestionId, Vec<f64>>,
}

/// Token counts of a decision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fed_tokens: Option<u64>,
}

/// The fields a fitted threshold depends on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionIdentity {
    pub model: String,
    pub weight_hash: String,
    pub tokenizer_hash: String,
    pub template: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_id: Option<SpecId>,
}

/// The reply to `POST /council/v1/decisions`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub model: String,
    pub answers: IndexMap<QuestionId, PooledAnswer>,
    pub reads: Vec<Read>,
    pub pool: PoolEcho,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signals: Option<Signals>,
    pub identity: DecisionIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
}

// ---- identity and errors

/// What a server can do beyond the base contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Park,
    Warm,
    DryRun,
    /// `PUT` accepts `persist`, and the server may drop a `persist: false`
    /// context at a restart.
    Persist,
    Describe,
    LeaveOneOut,
    /// A capability this client does not know. A server may add capabilities;
    /// a client built before one keeps working and never uses it.
    #[serde(other)]
    Unknown,
}

/// A server's limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub context_tokens: u64,
    pub state_bytes: u64,
    pub contexts_per_decision: u64,
    pub choice_options: u64,
    pub default_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions_per_spec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_bytes: Option<u64>,
}

/// The reply to `GET /council/v1/identity`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIdentity {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    pub weight_hash: String,
    pub tokenizer_hash: String,
    pub template: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_stop: Option<String>,
    pub limits: Limits,
    pub capabilities: Vec<Capability>,
}

/// The `type` of an error body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    InvalidRequest,
    NotFound,
    SnapshotGone,
    HeadMismatch,
    TooLarge,
    Busy,
    Unavailable,
    Timeout,
    PinBudget,
    Internal,
}

/// The inside of an error body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub r#type: ErrorType,
    pub message: String,
    /// The request field or limit at fault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// For `snapshot_gone` and `head_mismatch`, the context's current head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<SnapshotId>,
}

/// An error body: `{"error": {...}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

/// JSON of the docs' examples, shared by tests.
#[cfg(test)]
pub(crate) mod tests_support {
    /// The spec of the docs' "Hold a spec once" example.
    pub const DOC_SPEC: &str = include_str!("../tests/fixtures/spec.json");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REQUEST: &str = include_str!("../tests/fixtures/decision_request.json");
    const RESPONSE: &str = include_str!("../tests/fixtures/decision_response.json");

    fn snap(c: char) -> String {
        format!("snap:{}", c.to_string().repeat(64))
    }

    #[test]
    fn the_docs_spec_decodes_and_keeps_option_order() {
        let spec: Spec = serde_json::from_str(tests_support::DOC_SPEC).unwrap();
        assert_eq!(spec.questions.len(), 3);
        let SpecQuestion::Choice(c) = &spec.questions[2] else { panic!("not a choice") };
        let names: Vec<&str> = c.criteria.iter().map(|o| o.option.as_str()).collect();
        assert_eq!(names, ["allow", "ask", "report"]);
        let SpecQuestion::Text(t) = &spec.questions[0] else { panic!("not text") };
        assert_eq!(t.max_tokens, 48);
    }

    #[test]
    fn a_spec_serializes_back_to_the_same_json() {
        let spec: Spec = serde_json::from_str(tests_support::DOC_SPEC).unwrap();
        let original: serde_json::Value = serde_json::from_str(tests_support::DOC_SPEC).unwrap();
        assert_eq!(serde_json::to_value(&spec).unwrap(), original);
    }

    #[test]
    fn the_docs_decision_request_decodes_validates_and_round_trips() {
        let req: DecisionRequest = serde_json::from_str(REQUEST).unwrap();
        req.validate().unwrap();
        assert_eq!(req.contexts.as_ref().unwrap().len(), 2);
        assert_eq!(req.pool.as_ref().unwrap().method, Some(PoolMethod::Loglinear));
        let original: serde_json::Value = serde_json::from_str(REQUEST).unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), original);
    }

    #[test]
    fn the_docs_decision_response_decodes_in_order() {
        let resp: DecisionResponse = serde_json::from_str(RESPONSE).unwrap();
        assert_eq!(resp.answers.keys().collect::<Vec<_>>(), ["undo", "verdict"]);
        let PooledAnswer::Choice(v) = &resp.answers["verdict"] else { panic!("not a choice") };
        assert_eq!(v.probabilities.keys().collect::<Vec<_>>(), ["allow", "ask", "report"]);
        assert_eq!(v.leave_one_out.as_ref().unwrap().len(), 2);
        assert_eq!(resp.reads.len(), 2);
        assert!(resp.reads[0].context.is_some());
    }

    #[test]
    fn a_response_tolerates_unknown_fields() {
        let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        v["extra"] = json!(1);
        v["reads"][0]["extra"] = json!({"a": 1});
        v["identity"]["extra"] = json!("x");
        v["answers"]["verdict"]["extra"] = json!(true);
        serde_json::from_value::<DecisionResponse>(v).unwrap();
    }

    #[test]
    fn a_response_missing_a_required_field_is_an_error() {
        for (path, name) in [
            ("", "model"),
            ("", "identity"),
            ("identity", "engine"),
            ("pool", "normalized"),
            ("reads/0", "rendered_sha256"),
            ("reads/0", "snapshot"),
            ("reads/0", "context"),
            ("reads/0/answers/verdict", "mass"),
            ("answers/verdict", "spread"),
        ] {
            let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
            let target = if path.is_empty() { &mut v } else { v.pointer_mut(&format!("/{path}")).unwrap() };
            target.as_object_mut().unwrap().remove(name);
            assert!(serde_json::from_value::<DecisionResponse>(v).is_err(), "{path}/{name} removed");
        }
    }

    #[test]
    fn a_null_context_and_snapshot_decode_as_none() {
        let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        v["reads"][0]["context"] = json!(null);
        v["reads"][0]["snapshot"] = json!(null);
        let resp: DecisionResponse = serde_json::from_value(v).unwrap();
        assert!(resp.reads[0].context.is_none() && resp.reads[0].snapshot.is_none());
    }

    #[test]
    fn a_response_with_a_wrong_type_is_an_error() {
        for (pointer, value) in [
            ("/model", json!(3)),
            ("/reads/0/answers/verdict/mass", json!("0")),
            ("/reads/0/answers/verdict/type", json!("rank")),
            ("/answers/verdict/agree", json!("yes")),
            ("/usage/input_tokens", json!(1.5)),
            ("/pool/method", json!("median")),
            ("/reads/0/snapshot", json!("snap:short")),
        ] {
            let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
            *v.pointer_mut(pointer).unwrap() = value;
            assert!(serde_json::from_value::<DecisionResponse>(v).is_err(), "{pointer}");
        }
    }

    #[test]
    fn a_context_put_omits_absent_optionals() {
        let put = ContextPut {
            system: "s".into(),
            turns: vec![
                Turn { role: Role::User, content: "a".into(), snap: false, reasoning: None },
                Turn { role: Role::Assistant, content: "b".into(), snap: true, reasoning: None },
            ],
            pin: None,
            warm: None,
            dry_run: None,
            persist: None,
        };
        assert_eq!(
            serde_json::to_value(&put).unwrap(),
            json!({"system": "s", "turns": [{"role": "user", "content": "a"},
                                             {"role": "assistant", "content": "b", "snap": true}]})
        );
        let full = ContextPut { pin: Some(false), dry_run: Some(true), ..put };
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(v["pin"], json!(false));
        assert_eq!(v["dry_run"], json!(true));
        assert!(v.get("warm").is_none());
    }

    #[test]
    fn a_context_put_carries_persist_and_the_capability_is_named_persist() {
        let put = ContextPut { system: "s".into(), turns: vec![], pin: None, warm: None, dry_run: None, persist: Some(false) };
        assert_eq!(serde_json::to_value(&put).unwrap()["persist"], json!(false));
        let none = ContextPut { persist: None, ..put };
        assert!(serde_json::to_value(&none).unwrap().get("persist").is_none());
        let caps: Vec<Capability> = serde_json::from_value(json!(["warm", "persist"])).unwrap();
        assert_eq!(caps, [Capability::Warm, Capability::Persist]);
    }

    /// A capability this client does not know is kept as `Unknown`, so a
    /// server that adds one does not fail every client built before it.
    /// Falsified by a closed enum: the identity does not decode.
    #[test]
    fn an_unknown_capability_does_not_fail_the_identity() {
        let caps: Vec<Capability> = serde_json::from_value(json!(["warm", "time_travel"])).unwrap();
        assert_eq!(caps, [Capability::Warm, Capability::Unknown]);
    }

    #[test]
    fn an_assistant_turn_carries_its_reasoning() {
        let turn = Turn { role: Role::Assistant, content: "hold".into(), snap: false, reasoning: Some("it opens the original".into()) };
        assert_eq!(
            serde_json::to_value(&turn).unwrap(),
            json!({"role": "assistant", "content": "hold", "reasoning": "it opens the original"})
        );
    }

    #[test]
    fn a_minimal_decision_request_serializes_only_its_fields() {
        let req = DecisionRequest {
            state: Json::from("git status"),
            spec_id: Some(SpecId::parse(format!("sha256:{}", "a".repeat(64))).unwrap()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"state": "git status", "spec_id": format!("sha256:{}", "a".repeat(64))})
        );
    }

    #[test]
    fn inline_questions_keep_member_order() {
        let text = r#"{"state":{"z":1,"a":2},"questions":{"q":{"type":"choice","instructions":"i","criteria":{"yes":"y","maybe":"m","no":"n"}},"b":{"type":"noul","instructions":"j"}}}"#;
        let req: DecisionRequest = serde_json::from_str(text).unwrap();
        req.validate().unwrap();
        assert_eq!(serde_json::to_string(&req).unwrap(), text);
        let Some(InlineQuestion::Choice(c)) = req.questions.as_ref().unwrap().get("q") else { panic!() };
        assert_eq!(c.criteria.keys().collect::<Vec<_>>(), ["yes", "maybe", "no"]);
    }

    #[test]
    fn validate_refuses_what_the_contract_forbids() {
        let spec_id = SpecId::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        let ok = DecisionRequest { state: Json::from("s"), spec_id: Some(spec_id.clone()), ..Default::default() };
        ok.validate().unwrap();
        let inline: IndexMap<String, InlineQuestion> = serde_json::from_value(
            json!({"q": {"type": "noul", "instructions": "i"}}),
        )
        .unwrap();
        let ctx = |id: &str| ContextRef { id: id.into(), at: None };
        let cases: Vec<(&str, DecisionRequest)> = vec![
            ("neither", DecisionRequest { spec_id: None, ..ok.clone() }),
            ("both", DecisionRequest { questions: Some(inline.clone()), ..ok.clone() }),
            ("ask with inline", DecisionRequest { spec_id: None, questions: Some(inline.clone()), ask: Some(vec!["q".into()]), ..ok.clone() }),
            ("state number", DecisionRequest { state: Json::Number(3.into()), ..ok.clone() }),
            ("repeat", DecisionRequest { contexts: Some(vec![ctx("a"), ctx("a")]), ..ok.clone() }),
            ("nine", DecisionRequest { contexts: Some((0..9).map(|i| ctx(&format!("c{i}"))).collect()), ..ok.clone() }),
            ("timeout 0", DecisionRequest { timeout_ms: Some(0), ..ok.clone() }),
            ("one option", DecisionRequest { options: Some(IndexMap::from([("q".to_string(), vec!["a".to_string()])])), ..ok.clone() }),
            ("given without values", DecisionRequest { pool: Some(Pool { weights: Some(PoolWeights::Given), ..Default::default() }), ..ok.clone() }),
            ("values without given", DecisionRequest { pool: Some(Pool { values: Some(vec![1.0]), ..Default::default() }), ..ok.clone() }),
            ("values wrong length", DecisionRequest { contexts: Some(vec![ctx("a"), ctx("b")]), pool: Some(Pool { weights: Some(PoolWeights::Given), values: Some(vec![1.0]), ..Default::default() }), ..ok.clone() }),
            ("values negative", DecisionRequest { contexts: Some(vec![ctx("a"), ctx("b")]), pool: Some(Pool { weights: Some(PoolWeights::Given), values: Some(vec![2.0, -1.0]), ..Default::default() }), ..ok.clone() }),
            ("values sum 0", DecisionRequest { contexts: Some(vec![ctx("a"), ctx("b")]), pool: Some(Pool { weights: Some(PoolWeights::Given), values: Some(vec![0.0, 0.0]), ..Default::default() }), ..ok.clone() }),
        ];
        for (name, req) in cases {
            assert!(req.validate().is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn ids_check_their_form() {
        assert!(SnapshotId::parse(snap('0')).is_ok());
        assert!(SnapshotId::parse("snap:abc").is_err());
        assert!(SnapshotId::parse(format!("snap:{}", "A".repeat(64))).is_err());
        assert!(SpecId::parse(snap('0')).is_err());
    }

    #[test]
    fn identity_decodes_and_keeps_an_unknown_capability_as_unknown() {
        let ok = json!({"model": "m", "weight_hash": "w", "tokenizer_hash": "t", "template": "x", "engine": "e",
                        "limits": {"context_tokens": 1, "state_bytes": 1, "contexts_per_decision": 8,
                                   "choice_options": 255, "default_timeout_ms": 1000},
                        "capabilities": ["park", "dry_run", "leave_one_out"], "future_field": 1});
        let id: ServerIdentity = serde_json::from_value(ok.clone()).unwrap();
        assert_eq!(id.capabilities, [Capability::Park, Capability::DryRun, Capability::LeaveOneOut]);
        let mut bad = ok.clone();
        bad["capabilities"] = json!(["teleport"]);
        let newer: ServerIdentity = serde_json::from_value(bad).unwrap();
        assert_eq!(newer.capabilities, [Capability::Unknown], "a newer server's capability is not an error");
        let mut missing = ok;
        missing["limits"].as_object_mut().unwrap().remove("state_bytes");
        assert!(serde_json::from_value::<ServerIdentity>(missing).is_err());
    }

    #[test]
    fn context_results_decode() {
        let v = json!({"id": "0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b11", "head": snap('1'), "tokens": 96, "kept": 0,
                       "fed": 96, "dry_run": false,
                       "snapshots": [{"id": snap('2'), "tokens": 40, "layer": "turn", "turn": 0, "pinned": true}]});
        let r: ContextPutResult = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(r.snapshots[0].layer, Layer::Turn);
        let mut no_fed = v;
        no_fed.as_object_mut().unwrap().remove("fed");
        assert!(serde_json::from_value::<ContextPutResult>(no_fed).is_err());
    }

    #[test]
    fn errors_decode_with_a_head() {
        let b: ErrorBody = serde_json::from_value(
            json!({"error": {"type": "head_mismatch", "message": "m", "head": snap('3')}}),
        )
        .unwrap();
        assert_eq!(b.error.r#type, ErrorType::HeadMismatch);
        assert_eq!(b.error.head.unwrap().as_str(), snap('3'));
    }
}
