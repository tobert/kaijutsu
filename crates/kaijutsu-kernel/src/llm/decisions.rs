//! A `decisions` backend: a hosted Decisions API, which takes application
//! state plus typed questions and returns probabilities instead of text.
//!
//! ```ignore
//! let provider = kernel.llm().read().await.get("jev").ok_or("no backend jev")?;
//! let client = provider.decisions().ok_or("jev is not a decisions backend")?;
//! let question = decisions::Choice::new(
//!     "Should this shell command run without asking a person?",
//!     [("allow", "read-only, or routine and easy to undo"),
//!      ("ask", "changes something a person should see first"),
//!      ("report", "harmful or plainly against the house rules")],
//! );
//! let answer = client
//!     .choose("typesafe/jev-1.13", "cmd: git push --force".into(), "verdict", &question, Duration::from_secs(10))
//!     .await?;
//! let p_allow = answer.p("allow"); // every option has a probability
//! ```
//!
//! Two APIs speak it, one [`Wire`] each. The backend's `--base-url` is the full
//! endpoint, and its path names the wire:
//!
//! - `https://openrouter.ai/api/alpha/decisions`: OpenRouter, bearer key, the
//!   state under `state`. Models `typesafe/jev-1.13` and `~typesafe/jev-latest`.
//! - `https://generativelanguage.googleapis.com/v1beta/decisions`: Gemini,
//!   `x-goog-api-key`, the state under `input` as text, and no `model` or
//!   `cost` in the reply. Model `gemini-decision-1`.
//!
//! Only `choice` questions are asked: one label among 2 to 255 options. The
//! providers do not check everything, so this module does:
//!
//! - a `choice` with one option is accepted and always answers `1.0`, so
//!   [`Choice`] refuses fewer than 2 options before any request leaves;
//! - unknown request fields are ignored, so the request body is built here
//!   from closed types and nothing else;
//! - a validation failure returns the provider's error list as a string in
//!   `error.message`, which [`DecideError::Provider`] carries verbatim.
//!
//! A reply that does not match the questions asked (a missing answer, another
//! kind of answer, a label outside the options, an option with no
//! probability, a probability outside 0 to 1) is an error naming the
//! question, never a filled-in value. A caller thresholding on a probability
//! must not read a made-up one.
//!
//! A decisions backend runs no chat turn: [`crate::llm::Provider::stream`]
//! refuses one, and a context's `model` slot may not name it. It serves a
//! `gate` slot.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::llm::endpoint::Endpoint;
use crate::llm::{LlmError, LlmResult, http_user_agent};

/// OpenRouter's Decisions endpoint.
pub const OPENROUTER_DECISIONS_URL: &str = "https://openrouter.ai/api/alpha/decisions";

/// Google's Gemini Decisions endpoint.
pub const GEMINI_DECISIONS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/decisions";

/// A `choice` may name at most this many options.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// Which Decisions API a backend speaks. The field names, the key header and
/// the reply differ; the question types are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// Bearer key, the state under `state`.
    OpenRouter,
    /// `x-goog-api-key`, the state under `input` as text.
    Gemini,
}

impl Wire {
    /// The wire a decisions endpoint speaks, from the end of its path:
    /// `/api/alpha/decisions` is OpenRouter and `/v1beta/decisions` is
    /// Gemini, on any host. Any other URL is refused, naming both.
    pub fn for_url(url: &str) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(url.trim()).map_err(|e| format!("{url} is not a URL: {e}"))?;
        let path = parsed.path().trim_end_matches('/');
        if path.ends_with("/api/alpha/decisions") {
            Ok(Self::OpenRouter)
        } else if path.ends_with("/v1beta/decisions") {
            Ok(Self::Gemini)
        } else {
            Err(format!(
                "{url} is not a Decisions endpoint; a decisions backend's --base-url is the full endpoint, \
                 {OPENROUTER_DECISIONS_URL} or {GEMINI_DECISIONS_URL} (another host with the same path works too)"
            ))
        }
    }

    /// The API's name, for `kj backend show`.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter Decisions API",
            Self::Gemini => "Gemini Decisions API",
        }
    }

    /// The environment variable a backend of this wire reads its key from
    /// when it names no key source.
    pub fn standard_env_var(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }

    /// The models this wire is known to serve.
    pub fn models(self) -> Vec<&'static str> {
        match self {
            Self::OpenRouter => vec!["typesafe/jev-1.13", "~typesafe/jev-latest"],
            Self::Gemini => vec!["gemini-decision-1"],
        }
    }
}

/// One `choice` question: pick one label among the options.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// The question, as the model reads it.
    pub instructions: String,
    /// Each option's label and what it means; 2 to 255 of them.
    pub options: BTreeMap<String, String>,
}

impl Choice {
    /// A question from its instructions and `(label, meaning)` pairs.
    pub fn new<K: Into<String>, V: Into<String>>(
        instructions: impl Into<String>,
        options: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            instructions: instructions.into(),
            options: options.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
        }
    }

    /// Refuses what the providers accept but answer wrongly: no instructions,
    /// fewer than 2 or more than [`MAX_CHOICE_OPTIONS`] options, an empty label.
    fn validate(&self, id: &str) -> Result<(), DecideError> {
        let bad = |why: String| DecideError::InvalidQuestion { id: id.to_string(), why };
        if self.instructions.trim().is_empty() {
            return Err(bad("its instructions are empty; say what to decide".into()));
        }
        let n = self.options.len();
        if !(2..=MAX_CHOICE_OPTIONS).contains(&n) {
            return Err(bad(format!(
                "a choice needs 2 to {MAX_CHOICE_OPTIONS} options, got {n}; with one option the answer is always 1.0"
            )));
        }
        if let Some(label) = self.options.keys().find(|l| l.trim().is_empty()) {
            return Err(bad(format!("option label {label:?} is empty")));
        }
        Ok(())
    }
}

/// A checked request: one model, the state, and the questions, on one wire.
#[derive(Debug, Clone)]
pub struct DecisionRequest {
    wire: Wire,
    model: String,
    state: serde_json::Value,
    questions: BTreeMap<String, Choice>,
}

impl DecisionRequest {
    /// Builds a request, refusing an empty question map or any invalid question.
    pub fn new(
        wire: Wire,
        model: impl Into<String>,
        state: serde_json::Value,
        questions: BTreeMap<String, Choice>,
    ) -> Result<Self, DecideError> {
        if questions.is_empty() {
            return Err(DecideError::NoQuestions);
        }
        for (id, q) in &questions {
            q.validate(id)?;
        }
        Ok(Self { wire, model: model.into(), state, questions })
    }

    /// The JSON body for this request's wire. Gemini reads its context as
    /// text, so a JSON `state` goes as its pretty-printed text and a string
    /// `state` as itself.
    pub fn body(&self) -> serde_json::Value {
        let questions: serde_json::Map<String, serde_json::Value> = self
            .questions
            .iter()
            .map(|(id, q)| {
                let question = serde_json::json!({
                    "type": "choice",
                    "instructions": q.instructions,
                    "criteria": q.options,
                });
                (id.clone(), question)
            })
            .collect();
        match self.wire {
            Wire::OpenRouter => serde_json::json!({
                "model": self.model,
                "state": self.state,
                "questions": questions,
            }),
            Wire::Gemini => {
                let input = match &self.state {
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string_pretty(other).expect("a JSON value serializes"),
                };
                serde_json::json!({
                    "model": self.model,
                    "input": input,
                    "questions": questions,
                })
            }
        }
    }
}

/// One `choice` answer, checked against its question.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChoiceAnswer {
    /// The label the provider picked.
    pub choice: String,
    /// Every option's probability, each from 0 to 1.
    pub probabilities: BTreeMap<String, f64>,
    /// The provider's confidence, when it reports one.
    #[serde(default)]
    pub confidence: Option<f64>,
}

impl ChoiceAnswer {
    /// Checks the answer against its question: the label is an option, and
    /// every option, and only an option, has a probability from 0 to 1.
    fn check(&self, id: &str, question: &Choice) -> Result<(), DecideError> {
        let mismatch = |why: String| DecideError::Mismatch { id: id.to_string(), why };
        if !question.options.contains_key(&self.choice) {
            return Err(mismatch(format!("the chosen label {:?} is not one of the options", self.choice)));
        }
        if let Some(label) = question.options.keys().find(|l| !self.probabilities.contains_key(*l)) {
            return Err(mismatch(format!("option {label:?} has no probability")));
        }
        if let Some(label) = self.probabilities.keys().find(|l| !question.options.contains_key(*l)) {
            return Err(mismatch(format!("the reply gives a probability for {label:?}, which is not an option")));
        }
        if let Some((label, p)) = self.probabilities.iter().find(|(_, p)| !(0.0..=1.0).contains(*p)) {
            return Err(mismatch(format!("option {label:?} has probability {p}, outside 0 to 1")));
        }
        Ok(())
    }

    /// The probability of `option`, or `None` for a label that was not an
    /// option. A checked answer has one for every option asked.
    pub fn p(&self, option: &str) -> Option<f64> {
        self.probabilities.get(option).copied()
    }
}

/// Token counts and cost as the provider reports them. Gemini spells the
/// counts `total_input_tokens` and `total_output_tokens` and reports no cost;
/// the plain name wins when a reply carries both.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(from = "RawUsage")]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// USD, when the provider reports it.
    pub cost: Option<f64>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    total_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_output_tokens: Option<u64>,
    #[serde(default)]
    cost: Option<f64>,
}

impl From<RawUsage> for DecisionUsage {
    fn from(r: RawUsage) -> Self {
        Self {
            input_tokens: r.input_tokens.or(r.total_input_tokens).unwrap_or(0),
            output_tokens: r.output_tokens.or(r.total_output_tokens).unwrap_or(0),
            cost: r.cost,
        }
    }
}

/// The provider's reply, checked against the request that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResponse {
    /// The versioned model that answered, such as
    /// `typesafe/jev-1.13-20260917`. Gemini names none, so it is the model
    /// asked for.
    pub model: String,
    pub answers: BTreeMap<String, ChoiceAnswer>,
    pub usage: DecisionUsage,
    /// The provider's request id, when it reports one.
    pub id: Option<String>,
}

impl DecisionResponse {
    /// Reads a reply body and checks every answer against `req`.
    pub fn parse(body: &str, req: &DecisionRequest) -> Result<Self, DecideError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            model: Option<String>,
            answers: BTreeMap<String, serde_json::Value>,
            #[serde(default)]
            usage: DecisionUsage,
            #[serde(default)]
            id: Option<String>,
        }
        let raw: Raw = serde_json::from_str(body).map_err(|e| DecideError::Unreadable(e.to_string()))?;
        let mismatch = |id: &str, why: String| DecideError::Mismatch { id: id.to_string(), why };
        if let Some(extra) = raw.answers.keys().find(|k| !req.questions.contains_key(*k)) {
            return Err(mismatch(extra, "the reply answers a question that was not asked".into()));
        }
        let mut answers = BTreeMap::new();
        for (id, question) in &req.questions {
            let Some(value) = raw.answers.get(id) else {
                return Err(mismatch(id, "the reply has no answer for it".into()));
            };
            let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("(none)");
            if kind != "choice" {
                return Err(mismatch(id, format!("asked for a choice answer, got type {kind}")));
            }
            let answer: ChoiceAnswer = serde_json::from_value(value.clone())
                .map_err(|e| mismatch(id, format!("the choice answer could not be read: {e}")))?;
            answer.check(id, question)?;
            answers.insert(id.clone(), answer);
        }
        Ok(Self {
            model: raw.model.filter(|m| !m.is_empty()).unwrap_or_else(|| req.model.clone()),
            answers,
            usage: raw.usage,
            id: raw.id,
        })
    }
}

/// Why a decision returned no answers.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecideError {
    #[error("no questions: a decision asks at least one")]
    NoQuestions,
    #[error("question {id:?} is invalid: {why}")]
    InvalidQuestion { id: String, why: String },
    /// The provider refused or failed the request. `message` is its own text.
    #[error("the decisions provider answered HTTP {status}: {message}")]
    Provider { status: u16, message: String },
    #[error("the provider's reply does not match question {id:?}: {why}")]
    Mismatch { id: String, why: String },
    #[error("the provider's reply could not be read: {0}")]
    Unreadable(String),
    #[error("the decision got no answer in time: {0}")]
    Timeout(String),
    #[error("the decisions request failed before an answer arrived: {0}")]
    Transport(String),
}

impl DecideError {
    /// Whether the same request may pass when sent again: a 429, a 5xx, a
    /// timeout, or a transport failure. Any other failure needs a different
    /// request.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Provider { status, .. } => *status == 429 || *status >= 500,
            Self::Timeout(_) | Self::Transport(_) => true,
            _ => false,
        }
    }
}

/// The provider's `{"error": {"message"}}` text, or the raw body when it has
/// none, so nothing the provider said is dropped.
fn provider_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
}

/// A decisions backend's client: one endpoint, one key, one wire.
#[derive(Clone)]
pub struct Client {
    name: String,
    wire: Wire,
    url: String,
    key: String,
    http: reqwest::Client,
    request_timeout: Duration,
    endpoint: Option<Arc<Endpoint>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("decisions::Client")
            .field("name", &self.name)
            .field("wire", &self.wire)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// `name` is the backend name; `url` is the full endpoint, whose path
    /// picks the wire ([`Wire::for_url`]). No call takes longer than
    /// `request_timeout`.
    pub fn new(name: impl Into<String>, url: &str, key: impl Into<String>, request_timeout: Duration) -> LlmResult<Self> {
        let name = name.into();
        let wire = Wire::for_url(url).map_err(|e| LlmError::InvalidRequest(format!("backend '{name}': {e}")))?;
        let key = key.into();
        if reqwest::header::HeaderValue::from_str(&key).is_err() {
            return Err(LlmError::AuthError(format!(
                "backend '{name}': the API key is not a valid HTTP header value (it holds a control character, \
                 such as a newline)"
            )));
        }
        let http = reqwest::Client::builder()
            .user_agent(http_user_agent())
            .build()
            .map_err(|e| LlmError::NetworkError(format!("backend '{name}': could not build an HTTP client: {e}")))?;
        Ok(Self { name, wire, url: url.trim().to_string(), key, http, request_timeout, endpoint: None })
    }

    /// Take a slot from `endpoint` before each call.
    pub fn with_endpoint(mut self, endpoint: Arc<Endpoint>) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// The endpoint this client takes slots from.
    pub fn endpoint(&self) -> Option<&Arc<Endpoint>> {
        self.endpoint.as_ref()
    }

    /// The backend name.
    pub fn provider_name(&self) -> &str {
        &self.name
    }

    pub fn wire(&self) -> Wire {
        self.wire
    }

    /// Asks `questions` about `state` of `model`, and returns the checked
    /// answers. The call, its wait for an endpoint slot included, ends after
    /// `timeout` or the backend's request timeout, whichever is shorter.
    pub async fn decide(
        &self,
        model: &str,
        state: serde_json::Value,
        questions: BTreeMap<String, Choice>,
        timeout: Duration,
    ) -> Result<DecisionResponse, DecideError> {
        let req = DecisionRequest::new(self.wire, model, state, questions)?;
        let timeout = timeout.min(self.request_timeout);
        let timed_out = || DecideError::Timeout(format!("backend '{}' gave no answer within {timeout:?}", self.name));
        let deadline = tokio::time::Instant::now() + timeout;
        let slot = crate::llm::endpoint::take_for_request(self.endpoint.as_ref(), timeout)
            .await
            .map_err(|e| DecideError::Timeout(format!("backend '{}' had no free slot: {e}", self.name)))?;
        let post = self.http.post(&self.url);
        let post = match self.wire {
            Wire::OpenRouter => post.bearer_auth(&self.key),
            Wire::Gemini => post.header("x-goog-api-key", &self.key),
        };
        let call = async {
            let resp = post.json(&req.body()).send().await.map_err(|e| DecideError::Transport(e.to_string()))?;
            crate::llm::endpoint::observe(slot.as_ref(), &resp);
            let status = resp.status();
            let body = resp.text().await.map_err(|e| DecideError::Transport(e.to_string()))?;
            Ok::<_, DecideError>((status, body))
        };
        let (status, body) = tokio::time::timeout_at(deadline, call).await.map_err(|_| timed_out())??;
        if !status.is_success() {
            return Err(DecideError::Provider { status: status.as_u16(), message: provider_message(&body) });
        }
        DecisionResponse::parse(&body, &req)
    }

    /// [`Self::decide`] for one question under `id`, returning its answer.
    pub async fn choose(
        &self,
        model: &str,
        state: serde_json::Value,
        id: &str,
        question: &Choice,
        timeout: Duration,
    ) -> Result<ChoiceAnswer, DecideError> {
        let mut response = self.decide(model, state, BTreeMap::from([(id.to_string(), question.clone())]), timeout).await?;
        response
            .answers
            .remove(id)
            .ok_or_else(|| DecideError::Mismatch { id: id.to_string(), why: "the reply has no answer for it".into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_council::test_server::{reply, serve};
    use serde_json::json;

    fn verdict() -> Choice {
        Choice::new("which", [("safe", "s"), ("risky", "r"), ("other", "o")])
    }

    fn one(id: &str, q: Choice) -> BTreeMap<String, Choice> {
        BTreeMap::from([(id.to_string(), q)])
    }

    #[test]
    fn the_url_path_picks_the_wire() {
        assert_eq!(Wire::for_url(OPENROUTER_DECISIONS_URL), Ok(Wire::OpenRouter));
        assert_eq!(Wire::for_url(GEMINI_DECISIONS_URL), Ok(Wire::Gemini));
        assert_eq!(Wire::for_url("http://127.0.0.1:9/v1beta/decisions/"), Ok(Wire::Gemini));
        assert_eq!(Wire::for_url("https://proxy.test/or/api/alpha/decisions"), Ok(Wire::OpenRouter));
        for bad in ["https://openrouter.ai/api/v1", "https://generativelanguage.googleapis.com", "not a url"] {
            let err = Wire::for_url(bad).unwrap_err();
            assert!(err.contains(bad), "{err}");
        }
        assert!(Wire::for_url("https://openrouter.ai").unwrap_err().contains(OPENROUTER_DECISIONS_URL));
    }

    #[test]
    fn a_choice_needs_two_to_255_options_before_any_request() {
        let only = Choice::new("which", [("only", "x")]);
        let err = DecisionRequest::new(Wire::OpenRouter, "m", "s".into(), one("q", only)).unwrap_err();
        assert!(matches!(&err, DecideError::InvalidQuestion { id, .. } if id == "q"), "{err}");
        assert!(err.to_string().contains("2 to 255"), "{err}");
        let many = Choice::new("which", (0..256).map(|i| (i.to_string(), "x")));
        assert!(DecisionRequest::new(Wire::OpenRouter, "m", "s".into(), one("q", many)).is_err());
        let blank_label = Choice::new("which", [(" ", "x"), ("b", "y")]);
        assert!(DecisionRequest::new(Wire::OpenRouter, "m", "s".into(), one("q", blank_label)).is_err());
        let no_instructions = Choice::new("  ", [("a", "x"), ("b", "y")]);
        assert!(DecisionRequest::new(Wire::Gemini, "m", "s".into(), one("q", no_instructions)).is_err());
        assert_eq!(
            DecisionRequest::new(Wire::Gemini, "m", "s".into(), BTreeMap::new()).unwrap_err(),
            DecideError::NoQuestions
        );
    }

    #[test]
    fn an_openrouter_request_sends_state_as_given() {
        let q = Choice::new("which", [("a", "first"), ("b", "second")]);
        let req = DecisionRequest::new(Wire::OpenRouter, "typesafe/jev-1.13", json!({"cmd": "ls"}), one("v", q)).unwrap();
        assert_eq!(
            req.body(),
            json!({
                "model": "typesafe/jev-1.13",
                "state": {"cmd": "ls"},
                "questions": {"v": {"type": "choice", "instructions": "which",
                                    "criteria": {"a": "first", "b": "second"}}}
            })
        );
    }

    #[test]
    fn a_gemini_request_sends_state_as_input_text() {
        let q = Choice::new("which", [("a", "first"), ("b", "second")]);
        let json_state = DecisionRequest::new(Wire::Gemini, "gemini-decision-1", json!({"cmd": "ls"}), one("v", q.clone()))
            .unwrap()
            .body();
        assert_eq!(
            json_state,
            json!({
                "model": "gemini-decision-1",
                "input": "{\n  \"cmd\": \"ls\"\n}",
                "questions": {"v": {"type": "choice", "instructions": "which",
                                    "criteria": {"a": "first", "b": "second"}}}
            })
        );
        let text_state = DecisionRequest::new(Wire::Gemini, "gemini-decision-1", "plain text".into(), one("v", q)).unwrap();
        assert_eq!(text_state.body()["input"], "plain text");
    }

    /// OpenRouter's reply to a choice, as a live probe recorded it.
    const OPENROUTER_REPLY: &str = r#"{"model":"typesafe/jev-1.13-20260917","answers":{
      "verdict":{"type":"choice","choice":"risky","probabilities":{"safe":0,"other":0,"risky":1},"confidence":1}},
      "usage":{"input_tokens":399,"output_tokens":72,"cost":0.000016758},
      "id":"gen-dec-1","provider":"TypeSafe"}"#;

    /// Gemini's reply to a choice, as a live probe recorded it.
    const GEMINI_REPLY: &str = r#"{"answers":{
      "verdict":{"confidence":0.89,"choice":"risky","probabilities":{"safe":0.06,"other":0.05,"risky":0.89},"type":"choice"}},
      "usage":{"total_tokens":295,"total_input_tokens":295,"input_tokens_by_modality":[{"modality":"text","tokens":295}],"total_output_tokens":0}}"#;

    fn request(wire: Wire) -> DecisionRequest {
        DecisionRequest::new(wire, "asked-model", "x".into(), one("verdict", verdict())).unwrap()
    }

    #[test]
    fn the_probed_replies_parse_and_check() {
        let r = DecisionResponse::parse(OPENROUTER_REPLY, &request(Wire::OpenRouter)).unwrap();
        assert_eq!(r.model, "typesafe/jev-1.13-20260917");
        assert_eq!(r.usage, DecisionUsage { input_tokens: 399, output_tokens: 72, cost: Some(0.000016758) });
        assert_eq!(r.id.as_deref(), Some("gen-dec-1"));
        assert_eq!(r.answers["verdict"].p("risky"), Some(1.0));

        let g = DecisionResponse::parse(GEMINI_REPLY, &request(Wire::Gemini)).unwrap();
        assert_eq!(g.model, "asked-model", "Gemini names no model, so it is the one asked for");
        assert_eq!(g.usage, DecisionUsage { input_tokens: 295, output_tokens: 0, cost: None });
        assert_eq!(g.answers["verdict"].confidence, Some(0.89));
        assert_eq!(g.answers["verdict"].p("safe"), Some(0.06));
    }

    /// Each way a reply can fail to match its question, and the text that
    /// names it.
    #[test]
    fn a_reply_that_does_not_match_the_question_is_an_error() {
        let base: serde_json::Value = serde_json::from_str(OPENROUTER_REPLY).unwrap();
        let cases: Vec<(&str, Box<dyn Fn(&mut serde_json::Value)>, &str)> = vec![
            ("missing", Box::new(|v| v["answers"] = json!({})), "no answer"),
            ("another kind", Box::new(|v| v["answers"]["verdict"] = json!({"type": "noul", "noul": 0.5})), "noul"),
            ("invented label", Box::new(|v| v["answers"]["verdict"]["choice"] = json!("invented")), "invented"),
            (
                "an option with no probability",
                Box::new(|v| v["answers"]["verdict"]["probabilities"] = json!({"safe": 0, "risky": 1})),
                "\"other\"",
            ),
            (
                "a probability for a label not asked",
                Box::new(|v| v["answers"]["verdict"]["probabilities"]["extra"] = json!(0)),
                "\"extra\"",
            ),
            ("a probability above 1", Box::new(|v| v["answers"]["verdict"]["probabilities"]["safe"] = json!(1.5)), "1.5"),
            ("an unasked question", Box::new(|v| v["answers"]["extra"] = v["answers"]["verdict"].clone()), "not asked"),
        ];
        for (case, edit, needle) in cases {
            let mut v = base.clone();
            edit(&mut v);
            let err = DecisionResponse::parse(&v.to_string(), &request(Wire::OpenRouter)).unwrap_err();
            assert!(matches!(err, DecideError::Mismatch { .. }), "{case}: {err}");
            assert!(err.to_string().contains(needle), "{case}: {err}");
        }
        let err = DecisionResponse::parse("<html>", &request(Wire::OpenRouter)).unwrap_err();
        assert!(matches!(err, DecideError::Unreadable(_)), "{err}");
    }

    #[test]
    fn the_provider_message_is_extracted_or_kept_raw() {
        assert_eq!(
            provider_message(r#"{"error":{"message":"Model typesafe/nope does not exist","code":400}}"#),
            "Model typesafe/nope does not exist"
        );
        assert_eq!(provider_message("<html>bad gateway</html>"), "<html>bad gateway</html>");
    }

    #[test]
    fn retryable_separates_busy_from_refused() {
        assert!(DecideError::Provider { status: 429, message: String::new() }.retryable());
        assert!(DecideError::Provider { status: 503, message: String::new() }.retryable());
        assert!(!DecideError::Provider { status: 400, message: String::new() }.retryable());
        assert!(!DecideError::Mismatch { id: "q".into(), why: String::new() }.retryable());
    }

    #[test]
    fn a_key_that_is_not_a_header_value_is_refused() {
        let err = Client::new("jev", OPENROUTER_DECISIONS_URL, "sk\nx", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, LlmError::AuthError(_)), "{err:?}");
        let err = Client::new("jev", "https://openrouter.ai/api/v1", "sk", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, LlmError::InvalidRequest(_)), "{err:?}");
    }

    /// One OpenRouter call end to end: the path, the bearer key, the body,
    /// and the checked answer.
    #[tokio::test]
    async fn an_openrouter_call_sends_the_key_and_returns_the_answer() {
        let (base, seen) = serve(vec![reply(200, OPENROUTER_REPLY)]).await;
        let client = Client::new("jev", &format!("{base}/api/alpha/decisions"), "sk-or", Duration::from_secs(5)).unwrap();
        let answer = client
            .choose("typesafe/jev-1.13", json!("cmd: rm -rf /"), "verdict", &verdict(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer.choice, "risky");
        let seen = seen.lock().unwrap();
        let req = &seen[0];
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/api/alpha/decisions"));
        assert_eq!(req.header("authorization"), Some("Bearer sk-or"));
        assert_eq!(req.header("x-goog-api-key"), None);
        assert_eq!(req.header("user-agent"), Some(http_user_agent()));
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["state"], "cmd: rm -rf /");
        assert_eq!(body["questions"]["verdict"]["criteria"]["risky"], "r");
    }

    #[tokio::test]
    async fn a_gemini_call_sends_the_goog_key_and_input() {
        let (base, seen) = serve(vec![reply(200, GEMINI_REPLY)]).await;
        let client = Client::new("gem", &format!("{base}/v1beta/decisions"), "AIza", Duration::from_secs(5)).unwrap();
        let answer = client
            .choose("gemini-decision-1", json!("cmd: ls"), "verdict", &verdict(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer.p("risky"), Some(0.89));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].header("x-goog-api-key"), Some("AIza"));
        assert_eq!(seen[0].header("authorization"), None);
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["input"], "cmd: ls");
        assert!(body.get("state").is_none(), "{body}");
    }

    /// The provider's validation error comes back verbatim, and an invalid
    /// question never reaches the provider.
    #[tokio::test]
    async fn provider_errors_pass_through_and_bad_questions_send_nothing() {
        let zod = r#"[{"code":"invalid_type","path":["questions","v","criteria"],"message":"Required"}]"#;
        let body = json!({"error": {"message": zod, "code": 400}}).to_string();
        let (base, seen) = serve(vec![reply(400, body)]).await;
        let client = Client::new("jev", &format!("{base}/api/alpha/decisions"), "k", Duration::from_secs(5)).unwrap();
        let err = client.choose("m", json!("s"), "v", &verdict(), Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(err, DecideError::Provider { status: 400, message: zod.into() });
        assert!(!err.retryable());

        let one_option = Choice::new("which", [("only", "x")]);
        let err = client.choose("m", json!("s"), "v", &one_option, Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, DecideError::InvalidQuestion { .. }), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1, "the invalid question sent nothing");
    }

    #[tokio::test]
    async fn a_slow_provider_times_out_at_the_callers_bound() {
        let slow = kaijutsu_council::test_server::Reply { delay: Duration::from_secs(5), ..reply(200, OPENROUTER_REPLY) };
        let (base, _) = serve(vec![slow]).await;
        let client = Client::new("jev", &format!("{base}/api/alpha/decisions"), "k", Duration::from_secs(30)).unwrap();
        let started = std::time::Instant::now();
        let err = client.choose("m", json!("s"), "verdict", &verdict(), Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, DecideError::Timeout(_)), "{err}");
        assert!(err.retryable());
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
