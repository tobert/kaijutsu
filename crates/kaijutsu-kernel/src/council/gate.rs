//! One council decision for a gated submission: the request, the deadline,
//! verification, the outcome, and its durable record. `docs/council.md` is
//! canonical.
//!
//! [`consult`] is the seam both gate paths call once the PreCall hooks have
//! let a submission continue: `run_gate` for the `shell_write` tool, and the
//! broker's gate-policy ask on the RPC shell paths. It returns `None`, and
//! the gate behaves exactly as it does without a council, unless every
//! condition holds: the origin is the shell gate or the gate-policy ask on
//! `shell_write`, `gate.toml` enables the council for the caller's context
//! type, the static evaluation escalates, no ask already holds the
//! submission, and no approved run of the same command is still unsettled.
//!
//! The outcome is decided by [`classify`], a pure function of the request,
//! the response, and the configuration:
//!
//! - a response whose numbers do not recompute, whose identity names another
//!   spec, or whose identity has no threshold is a **miss** (the contract
//!   leaves `identity.spec_id` optional, so only a named spec is compared);
//! - any control-text hit is an **ask**;
//! - any read's verdict mass below the floor is a **miss**;
//! - pooled p(allow) at or above `allow_at`, with agreement when the config
//!   requires it, is an **allow**;
//! - a pooled argmax of `report` is a **report**; anything else is an
//!   **ask**.
//!
//! A transport, status, decode or deadline failure is a miss with its cause.
//! Only an allow changes what the gate does, and only for statements no
//! static layer covered.

use std::time::{Duration, Instant};

use approval_ledger::council::{
    CouncilAgreement, CouncilControlText, CouncilDescribed, CouncilOption, CouncilOutcome, CouncilPooled,
    CouncilQuestion, CouncilRead, CouncilServer, CouncilThreshold as RecordedThreshold, NewCouncilDecision,
};
use approval_ledger::types::{AskVerdict, NewSignal, Origin, SignalSourceKind, SignalVerdict};
use kaijutsu_council::wire::{
    ContextRef, DecisionIdentity, DecisionRequest, DecisionResponse, Pool, PoolMethod, PoolWeights, PooledAnswer,
    ReadAnswer,
};
use kaijutsu_council::{CouncilError, Json};
use kaijutsu_types::ContextId;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::Instrument;

use super::sync::{PrepareMiss, Prepared};
use crate::kj::gate::GateSpec;
use crate::kj::gate_policy::{
    CouncilCase, CouncilConfig, CouncilIdentity, CouncilPoolMethod, CouncilPoolWeights, CouncilSpec,
    CouncilThreshold, GateConfigLoad, Layers, PolicyVerdict,
};
use crate::kj::KjCaller;

/// The spec question whose answer the gate acts on: a `choice` over
/// `allow`, `ask` and `report`.
pub(crate) const VERDICT: &str = "verdict";

/// The tool whose gate-policy ask the council may decide on the RPC paths.
const SHELL_WRITE: &str = "shell_write";

/// The submission a council decision reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Submission<'a> {
    /// The submitted command, whole.
    pub(crate) command: &'a str,
    /// Its plan, one entry per top-level statement.
    pub(crate) planned: &'a [kaish_kernel::PlannedStatement],
    pub(crate) context_type: Option<&'a str>,
    pub(crate) cwd: Option<&'a str>,
}

#[derive(Serialize)]
struct CaseState<'a> {
    command: &'a str,
    statements: Vec<CaseStatement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
}

#[derive(Serialize)]
struct CaseStatement {
    index: usize,
    rendered: String,
    kind: String,
    /// One per command, as `KJ_TOOL_PLAN` gives each command its `clause`.
    clauses: Vec<String>,
}

/// The case's `state`: the command whole, each statement with the clauses
/// a classifier reads, the seat's context type, and its working directory,
/// with members in that order.
pub(crate) fn case_state(submission: &Submission<'_>) -> Json {
    let state = CaseState {
        command: submission.command,
        statements: submission
            .planned
            .iter()
            .map(|s| CaseStatement {
                index: s.index,
                rendered: s.plan.rendered.clone(),
                kind: s.plan.statement_kind.clone(),
                clauses: crate::kj::plan_clauses::command_clause_texts(s),
            })
            .collect(),
        context_type: submission.context_type,
        cwd: submission.cwd,
    };
    // A struct serializes its fields in declaration order, and `Json` keeps
    // the order it reads, so the round trip is the ordered object.
    let text = serde_json::to_string(&state).expect("the case state is plain data");
    serde_json::from_str(&text).expect("serde_json output is JSON")
}

/// The decision request for `state` against the prepared contexts, each
/// pinned to the head the kernel prepared.
pub(crate) fn decision_request(prepared: &Prepared, council: &CouncilConfig, state: Json) -> DecisionRequest {
    DecisionRequest {
        spec_id: Some(prepared.spec_id.clone()),
        contexts: Some(
            prepared
                .contexts
                .iter()
                .map(|c| ContextRef { id: c.context_id.to_string(), at: Some(c.head.clone()) })
                .collect(),
        ),
        state,
        pool: Some(Pool {
            method: Some(match council.pool_method {
                CouncilPoolMethod::Linear => PoolMethod::Linear,
                CouncilPoolMethod::LogLinear => PoolMethod::Loglinear,
            }),
            weights: Some(match council.pool_weights {
                CouncilPoolWeights::Uniform => PoolWeights::Uniform,
                CouncilPoolWeights::Mass => PoolWeights::Mass,
            }),
            values: None,
        }),
        timeout_ms: Some(council.deadline_ms),
        ..Default::default()
    }
}

/// How a council decision ended, as the gate acts on it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Allow,
    Ask,
    Report,
    /// No usable answer; the cause in plain words.
    Miss(String),
}

impl Outcome {
    fn recorded(&self) -> (CouncilOutcome, Option<String>) {
        match self {
            Self::Allow => (CouncilOutcome::Allow, None),
            Self::Ask => (CouncilOutcome::Ask, None),
            Self::Report => (CouncilOutcome::Report, None),
            Self::Miss(cause) => (CouncilOutcome::Miss, Some(cause.clone())),
        }
    }
}

/// The pooled answer to [`VERDICT`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PooledVerdict {
    /// Option and pooled probability, in the spec's order.
    pub(crate) probabilities: Vec<(String, f64)>,
    /// The argmax the server named.
    pub(crate) choice: String,
    pub(crate) agree: bool,
    pub(crate) spread: f64,
}

impl PooledVerdict {
    pub(crate) fn p(&self, option: &str) -> Option<f64> {
        self.probabilities.iter().find(|(o, _)| o == option).map(|(_, p)| *p)
    }
}

/// What [`classify`] found.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Classification {
    pub(crate) outcome: Outcome,
    /// The threshold judged against, when the identity has one.
    pub(crate) threshold: Option<CouncilThreshold>,
    /// The pooled verdict, when the response carried one.
    pub(crate) verdict: Option<PooledVerdict>,
}

fn identity_of(identity: &DecisionIdentity) -> CouncilIdentity {
    CouncilIdentity {
        weight_hash: identity.weight_hash.clone(),
        engine: identity.engine.clone(),
        tokenizer_hash: identity.tokenizer_hash.clone(),
        template: identity.template.clone(),
    }
}

fn pooled_verdict(response: &DecisionResponse) -> Option<PooledVerdict> {
    match response.answers.get(VERDICT)? {
        PooledAnswer::Choice(c) => Some(PooledVerdict {
            probabilities: c.probabilities.iter().map(|(o, p)| (o.clone(), *p)).collect(),
            choice: c.choice.clone(),
            agree: c.agree,
            spread: c.spread,
        }),
        PooledAnswer::Score(_) | PooledAnswer::Noul(_) => None,
    }
}

/// Decide what a decoded response means for the gate. Pure: the request,
/// the response, the council settings, and the spec's name decide it.
pub(crate) fn classify(
    request: &DecisionRequest,
    response: &DecisionResponse,
    council: &CouncilConfig,
    spec_name: &str,
) -> Classification {
    let verdict = pooled_verdict(response);
    let miss = |cause: String, threshold: Option<CouncilThreshold>| Classification {
        outcome: Outcome::Miss(cause),
        threshold,
        verdict: verdict.clone(),
    };
    if let Err(mismatch) = kaijutsu_council::math::verify(response, request) {
        return miss(format!("the answer's numbers do not recompute ({mismatch})"), None);
    }
    if let (Some(read), Some(asked)) = (&response.identity.spec_id, &request.spec_id)
        && read != asked
    {
        return miss(format!("the server read spec {read}, not the spec asked for, {asked}"), None);
    }
    let identity = identity_of(&response.identity);
    let Some(threshold) = council.threshold_for(spec_name, &identity) else {
        return miss(
            format!(
                "no threshold for spec {spec_name} under the server's identity (weight_hash {}, \
                 engine {}, tokenizer_hash {}, template {}); confirm or refit one in gate.toml",
                identity.weight_hash, identity.engine, identity.tokenizer_hash, identity.template
            ),
            None,
        );
    };
    let threshold = threshold.clone();
    let Some(pooled) = verdict.clone() else {
        return miss(format!("the answer has no `{VERDICT}` choice"), Some(threshold));
    };
    let Some(p_allow) = pooled.p("allow") else {
        return miss(format!("the `{VERDICT}` choice has no `allow` option"), Some(threshold));
    };
    let hits = response.signals.as_ref().map(|s| s.control_text.len()).unwrap_or(0);
    if hits > 0 {
        return Classification { outcome: Outcome::Ask, threshold: Some(threshold), verdict };
    }
    for read in &response.reads {
        let mass = match read.answers.get(VERDICT) {
            Some(ReadAnswer::Choice(c)) => c.mass,
            _ => {
                return miss(
                    format!("the read of {} has no `{VERDICT}` choice", read.context.as_deref().unwrap_or("the spec")),
                    Some(threshold),
                )
            }
        };
        if mass < threshold.mass_floor {
            return miss(
                format!(
                    "low mass: the read of {} put log probability {mass:.4} on the verdict's options, \
                     below the floor {}",
                    read.context.as_deref().unwrap_or("the spec"),
                    threshold.mass_floor
                ),
                Some(threshold),
            );
        }
    }
    let outcome = if p_allow >= threshold.allow_at && (!council.require_agree || pooled.agree) {
        Outcome::Allow
    } else if pooled.choice == "report" {
        Outcome::Report
    } else {
        Outcome::Ask
    };
    Classification { outcome, threshold: Some(threshold), verdict }
}

/// A council failure that produced no response, in plain words.
pub(crate) fn failure_cause(error: &CouncilError, deadline_ms: u64) -> String {
    match error {
        CouncilError::Timeout => format!("no answer within the {deadline_ms} ms deadline"),
        CouncilError::Status { status, error: Some(detail), .. } => {
            format!("the council server answered {status} ({:?}: {})", detail.r#type, detail.message)
        }
        CouncilError::Status { status, body, .. } => {
            format!("the council server answered {status}: {}", body.chars().take(200).collect::<String>())
        }
        CouncilError::Transport(e) => format!("the council server could not be reached: {e}"),
        CouncilError::Decode { what, message, .. } => format!("the answer is outside the schema ({what}): {message}"),
        CouncilError::Request(e) => format!("the kernel built a request the contract refuses: {e}"),
        CouncilError::SpecIdMismatch { computed, server } => {
            format!("the server holds the spec as {server}, not {computed}")
        }
    }
}

/// The contexts a 404 or 409 names, or every prepared one when it names
/// none: the server forgot what it held, and sending a context again is
/// idempotent.
fn contexts_to_resend(error: &CouncilError, prepared: &Prepared) -> Vec<ContextId> {
    let Some(404 | 409) = error.status() else { return Vec::new() };
    let text = match error {
        CouncilError::Status { error: Some(d), body, .. } => format!("{} {} {body}", d.message, d.param.as_deref().unwrap_or("")),
        CouncilError::Status { body, .. } => body.clone(),
        _ => String::new(),
    };
    let named: Vec<ContextId> = prepared
        .contexts
        .iter()
        .filter(|c| text.contains(&c.context_id.to_string()))
        .map(|c| c.context_id)
        .collect();
    if named.is_empty() { prepared.contexts.iter().map(|c| c.context_id).collect() } else { named }
}

/// A council verdict for the gate, with the record and the signal it leaves.
#[derive(Clone, Debug)]
pub(crate) struct CouncilVerdict {
    /// The spec's name, as `gate.toml` declares it.
    pub(crate) spec: String,
    pub(crate) outcome: Outcome,
    /// The pooled p(allow), when the council answered.
    pub(crate) p_allow: Option<f64>,
    record: NewCouncilDecision,
    signal: NewSignal,
    note: String,
}

impl CouncilVerdict {
    /// Whether the council allows the statements no static layer covered.
    pub(crate) fn allows(&self) -> bool {
        self.outcome == Outcome::Allow
    }

    /// The durable record, linked to the ask it led to when there is one.
    pub(crate) fn record_for(&self, request_id: Option<&str>) -> NewCouncilDecision {
        let mut record = self.record.clone();
        record.request_id = request_id.map(str::to_owned);
        record
    }

    /// The signal an ask carries, so `kj ledger show --signals` shows the
    /// council's answer.
    pub(crate) fn signal(&self) -> NewSignal {
        self.signal.clone()
    }

    /// One line for the ask's description.
    pub(crate) fn note(&self) -> &str {
        &self.note
    }

    /// The key an allowed statement's decision names.
    pub(crate) fn allow_key(&self) -> String {
        format!("at p={:.3}", self.p_allow.unwrap_or(f64::NAN))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The record's submission digest: the submitted command, hashed.
fn submission_digest(command: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(command.as_bytes())))
}

fn pool_words(council: &CouncilConfig) -> (&'static str, &'static str) {
    (
        match council.pool_method {
            CouncilPoolMethod::Linear => "linear",
            CouncilPoolMethod::LogLinear => "loglinear",
        },
        match council.pool_weights {
            CouncilPoolWeights::Uniform => "uniform",
            CouncilPoolWeights::Mass => "mass",
        },
    )
}

fn read_question(id: &str, answer: &ReadAnswer) -> CouncilQuestion {
    let options = |logprobs: &mut dyn Iterator<Item = (&String, &f64)>, mass: f64| {
        logprobs
            .map(|(o, lp)| CouncilOption { option: o.clone(), logprob: *lp, probability: (lp - mass).exp() })
            .collect::<Vec<_>>()
    };
    match answer {
        ReadAnswer::Choice(c) => CouncilQuestion {
            question_id: id.to_string(),
            mass: c.mass,
            confidence: Some(c.confidence),
            options: options(&mut c.logprobs.iter(), c.mass),
        },
        ReadAnswer::Score(c) => CouncilQuestion {
            question_id: id.to_string(),
            mass: c.mass,
            confidence: Some(c.confidence),
            options: options(&mut c.logprobs.iter(), c.mass),
        },
        ReadAnswer::Noul(c) => CouncilQuestion {
            question_id: id.to_string(),
            mass: c.mass,
            confidence: None,
            options: options(&mut c.logprobs.iter(), c.mass),
        },
    }
}

fn pooled_rows(response: &DecisionResponse) -> Vec<CouncilPooled> {
    let mut rows = Vec::new();
    for (q, answer) in &response.answers {
        let mut push = |option: &str, probability: f64| {
            rows.push(CouncilPooled { question_id: q.clone(), option: option.to_string(), probability })
        };
        match answer {
            PooledAnswer::Choice(c) => c.probabilities.iter().for_each(|(o, p)| push(o, *p)),
            PooledAnswer::Score(c) => c.probabilities.iter().for_each(|(o, p)| push(o, *p)),
            PooledAnswer::Noul(c) => {
                push("yes", c.noul);
                push("no", 1.0 - c.noul);
            }
        }
    }
    rows
}

/// What the gate knows about the server when a decision ends.
enum Seen<'a> {
    /// `prepare` failed: no identity, no spec id.
    Nothing,
    /// The server was prepared but sent no usable response.
    Prepared(&'a Prepared),
    /// A decoded response.
    Answered(&'a Prepared, &'a DecisionResponse),
}

#[allow(clippy::too_many_arguments)]
fn build_verdict(
    caller: &KjCaller,
    submission: &Submission<'_>,
    council: &CouncilConfig,
    spec: &CouncilSpec,
    seen: Seen<'_>,
    classification: Classification,
    elapsed: Duration,
) -> CouncilVerdict {
    let (outcome, miss_cause) = classification.outcome.recorded();
    let (pool_method, pool_weights) = pool_words(council);
    let server = match &seen {
        Seen::Answered(_, r) => CouncilServer {
            model: r.identity.model.clone(),
            weight_hash: r.identity.weight_hash.clone(),
            tokenizer_hash: r.identity.tokenizer_hash.clone(),
            template: r.identity.template.clone(),
            engine: r.identity.engine.clone(),
        },
        Seen::Prepared(p) => CouncilServer {
            model: p.identity.model.clone(),
            weight_hash: p.identity.weight_hash.clone(),
            tokenizer_hash: p.identity.tokenizer_hash.clone(),
            template: p.identity.template.clone(),
            engine: p.identity.engine.clone(),
        },
        // The schema holds these NOT NULL; an empty string is the identity
        // nobody reported, and the miss cause says why.
        Seen::Nothing => CouncilServer {
            model: String::new(),
            weight_hash: String::new(),
            tokenizer_hash: String::new(),
            template: String::new(),
            engine: String::new(),
        },
    };
    let spec_id = match &seen {
        Seen::Answered(p, _) | Seen::Prepared(p) => p.spec_id.to_string(),
        Seen::Nothing => String::new(),
    };
    let (reads, pooled, control_text, queue_ms) = match &seen {
        Seen::Answered(prepared, response) => {
            let reads = response
                .reads
                .iter()
                .map(|read| {
                    let expected = read.context.as_deref().and_then(|id| {
                        prepared.contexts.iter().find(|c| c.context_id.to_string() == id).map(|c| c.head.to_string())
                    });
                    CouncilRead {
                        context_id: read
                            .context
                            .as_deref()
                            .map(|id| ContextId::parse(id).map(|c| c.as_bytes().to_vec()).unwrap_or_else(|_| id.as_bytes().to_vec())),
                        snapshot: read.snapshot.as_ref().map(|s| s.to_string()).unwrap_or_default(),
                        expected_head: expected,
                        rendered_sha256: read.rendered_sha256.clone(),
                        questions: read.answers.iter().map(|(q, a)| read_question(q, a)).collect(),
                        described: read
                            .described
                            .iter()
                            .flatten()
                            .map(|(q, body)| CouncilDescribed { question_id: q.clone(), body: body.clone() })
                            .collect(),
                    }
                })
                .collect();
            let control = response
                .signals
                .iter()
                .flat_map(|s| s.control_text.iter())
                .map(|hit| CouncilControlText { location: hit.r#where.clone(), token: hit.token.clone() })
                .collect();
            (reads, pooled_rows(response), control, response.queue_ms.unwrap_or(0.0).round() as i64)
        }
        Seen::Prepared(_) | Seen::Nothing => (Vec::new(), Vec::new(), Vec::new(), 0),
    };
    let threshold = classification.threshold.as_ref().map(|t| RecordedThreshold {
        allow_at: t.allow_at,
        mass_floor: t.mass_floor,
        require_agree: council.require_agree,
    });
    let verdict = classification.verdict.as_ref();
    // A miss is never an answer, so its signal carries no probability; the
    // record keeps whatever numbers the server sent.
    let p_allow = match classification.outcome {
        Outcome::Miss(_) => None,
        _ => verdict.and_then(|v| v.p("allow")),
    };
    let record = NewCouncilDecision {
        request_id: None,
        context_id: caller.context_id.map(|c| c.as_bytes().to_vec()).unwrap_or_default(),
        principal_id: caller.principal_id.as_bytes().to_vec(),
        submission_digest: submission_digest(submission.command),
        spec_id,
        spec_name: spec.name.clone(),
        server: server.clone(),
        pool_method: pool_method.to_string(),
        pool_weights: pool_weights.to_string(),
        threshold,
        deadline_ms: council.deadline_ms as i64,
        outcome,
        miss_cause: miss_cause.clone(),
        agreement: verdict.map(|v| CouncilAgreement { agree: v.agree, spread: v.spread }),
        queue_ms,
        ms: elapsed.as_millis() as i64,
        reads,
        pooled,
        control_text,
    };
    let label = match &classification.outcome {
        Outcome::Miss(cause) => format!("miss: {cause}"),
        other => other_word(other).to_string(),
    };
    let signal = NewSignal {
        source_kind: SignalSourceKind::Council,
        source_id: Some(spec.name.clone()),
        model_id: (!server.model.is_empty()).then(|| server.model.clone()),
        weight_hash: (!server.weight_hash.is_empty()).then(|| server.weight_hash.clone()),
        stmt_seq: None,
        cmd_seq: None,
        label: Some(label),
        score: p_allow,
        verdict: if classification.outcome == Outcome::Allow { SignalVerdict::Allow } else { SignalVerdict::Escalate },
    };
    let note = match (&classification.outcome, verdict) {
        (Outcome::Miss(cause), _) => format!("council ({}) gave no answer: {cause}", spec.name),
        (other, Some(v)) => format!(
            "council ({}) answered {}: {}",
            spec.name,
            other_word(other),
            v.probabilities.iter().map(|(o, p)| format!("p({o})={p:.3}")).collect::<Vec<_>>().join(", ")
        ),
        (other, None) => format!("council ({}) answered {}", spec.name, other_word(other)),
    };
    CouncilVerdict { spec: spec.name.clone(), outcome: classification.outcome, p_allow, record, signal, note }
}

fn other_word(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Allow => "allow",
        Outcome::Ask => "ask",
        Outcome::Report => "report",
        Outcome::Miss(_) => "miss",
    }
}

/// Ask the council once about `submission` and return its verdict. Every
/// failure is a verdict too: a miss with its cause. Runs with no database
/// lock held; the deadline bounds the decision call.
pub(crate) async fn decide(
    kernel: &crate::Kernel,
    caller: &KjCaller,
    submission: Submission<'_>,
    council: &CouncilConfig,
    spec: &CouncilSpec,
) -> CouncilVerdict {
    let span = tracing::info_span!(
        "council.decide",
        council.spec = %spec.name,
        council.outcome = tracing::field::Empty,
        council.miss_cause = tracing::field::Empty,
        council.p_allow = tracing::field::Empty,
        council.p_ask = tracing::field::Empty,
        council.p_report = tracing::field::Empty,
        council.agree = tracing::field::Empty,
        council.spread = tracing::field::Empty,
        council.model = tracing::field::Empty,
        council.weight_hash = tracing::field::Empty,
        council.engine = tracing::field::Empty,
        council.tokenizer_hash = tracing::field::Empty,
        council.template = tracing::field::Empty,
        council.spec_id = tracing::field::Empty,
        council.deadline_ms = council.deadline_ms,
        council.prepare_ms = tracing::field::Empty,
        council.ms = tracing::field::Empty,
        council.server_ms = tracing::field::Empty,
        council.queue_ms = tracing::field::Empty,
        context.id = tracing::field::Empty,
        principal.id = %caller.principal_id,
        actor.id = %caller.actor_id,
    );
    if let Some(context) = caller.context_id {
        span.record("context.id", context.to_string());
    }
    let verdict = decide_inner(kernel, caller, submission, council, spec).instrument(span.clone()).await;
    span.record("council.outcome", other_word(&verdict.outcome));
    if let Outcome::Miss(cause) = &verdict.outcome {
        span.record("council.miss_cause", cause.as_str());
    }
    verdict
}

async fn decide_inner(
    kernel: &crate::Kernel,
    caller: &KjCaller,
    submission: Submission<'_>,
    council: &CouncilConfig,
    spec: &CouncilSpec,
) -> CouncilVerdict {
    let span = tracing::Span::current();
    let started = Instant::now();
    let prepared = match kernel.council_sync().prepare(kernel, council, spec).await {
        Ok(prepared) => prepared,
        Err(PrepareMiss(cause)) => {
            span.record("council.prepare_ms", started.elapsed().as_millis() as u64);
            let classification = Classification { outcome: Outcome::Miss(cause), threshold: None, verdict: None };
            return build_verdict(caller, &submission, council, spec, Seen::Nothing, classification, started.elapsed());
        }
    };
    span.record("council.prepare_ms", started.elapsed().as_millis() as u64);
    span.record("council.model", prepared.identity.model.as_str());
    span.record("council.spec_id", prepared.spec_id.as_str());
    if let Some(cause) = spec_lacks_verdict(&prepared.spec) {
        let classification = Classification { outcome: Outcome::Miss(cause), threshold: None, verdict: None };
        return build_verdict(caller, &submission, council, spec, Seen::Prepared(&prepared), classification, started.elapsed());
    }

    let request = decision_request(&prepared, council, case_state(&submission));
    let (traceparent, _) = kaijutsu_telemetry::inject_trace_context();
    let traceparent = (!traceparent.is_empty()).then_some(traceparent);
    let deadline = Duration::from_millis(council.deadline_ms);
    let asked = Instant::now();
    let answer = tokio::time::timeout(
        deadline,
        prepared.client.decide_traced(&request, deadline, traceparent.as_deref()),
    )
    .await;
    let elapsed = asked.elapsed();
    span.record("council.ms", elapsed.as_millis() as u64);

    let response = match answer {
        Err(_) => Err(format!("no answer within the {} ms deadline", council.deadline_ms)),
        Ok(Err(error)) => {
            for context in contexts_to_resend(&error, &prepared) {
                kernel.council_sync().invalidate(context);
            }
            Err(failure_cause(&error, council.deadline_ms))
        }
        Ok(Ok(response)) => Ok(response),
    };
    let response = match response {
        Ok(response) => response,
        Err(cause) => {
            let classification = Classification { outcome: Outcome::Miss(cause), threshold: None, verdict: None };
            return build_verdict(caller, &submission, council, spec, Seen::Prepared(&prepared), classification, elapsed);
        }
    };

    let classification = classify(&request, &response, council, &spec.name);
    span.record("council.weight_hash", response.identity.weight_hash.as_str());
    span.record("council.engine", response.identity.engine.as_str());
    span.record("council.tokenizer_hash", response.identity.tokenizer_hash.as_str());
    span.record("council.template", response.identity.template.as_str());
    if let Some(ms) = response.ms {
        span.record("council.server_ms", ms);
    }
    if let Some(ms) = response.queue_ms {
        span.record("council.queue_ms", ms);
    }
    if let Some(v) = &classification.verdict {
        for (option, field) in [("allow", "council.p_allow"), ("ask", "council.p_ask"), ("report", "council.p_report")] {
            if let Some(p) = v.p(option) {
                span.record(field, p);
            }
        }
        span.record("council.agree", v.agree);
        span.record("council.spread", v.spread);
    }
    if classification.outcome == Outcome::Report {
        let answers: Vec<serde_json::Value> = response
            .reads
            .iter()
            .map(|read| {
                let label = read.context.as_deref().and_then(|id| {
                    prepared.contexts.iter().find(|c| c.context_id.to_string() == id).map(|c| c.label.clone())
                });
                let probabilities = match read.answers.get(VERDICT) {
                    Some(ReadAnswer::Choice(c)) => serde_json::json!(c.probabilities),
                    _ => serde_json::Value::Null,
                };
                serde_json::json!({"context": read.context, "label": label, "verdict": probabilities})
            })
            .collect();
        tracing::warn!(
            name: "council.report",
            target: "kaijutsu::council",
            spec = %spec.name,
            context_id = %caller.context_id.map(|c| c.to_string()).unwrap_or_default(),
            actor_id = %caller.actor_id,
            principal_id = %caller.principal_id,
            submission = %submission.command,
            answers = %serde_json::Value::Array(answers),
            "the council reports a shell submission; it goes to the ledger as an ask"
        );
    }
    build_verdict(caller, &submission, council, spec, Seen::Answered(&prepared, &response), classification, elapsed)
}

/// Why a spec cannot decide a gate ask, when it cannot: the gate acts on a
/// [`VERDICT`] choice with an `allow` option, and a spec without one would
/// only ever miss.
fn spec_lacks_verdict(spec: &kaijutsu_council::wire::Spec) -> Option<String> {
    use kaijutsu_council::wire::SpecQuestion;
    match spec.questions.iter().find(|q| q.id() == VERDICT) {
        Some(SpecQuestion::Choice(c)) if c.criteria.iter().any(|o| o.option == "allow") => None,
        Some(_) => Some(format!("spec {} has no `allow` option on its `{VERDICT}` choice", spec.name)),
        None => Some(format!("spec {} has no `{VERDICT}` choice", spec.name)),
    }
}

/// Whether a gate ask is one the council may decide: the shell gate, or the
/// gate-policy ask on `shell_write` that the RPC shell paths open. Another
/// hook's ask, a result review, and a `kj` verb are never the council's.
pub(crate) fn eligible(spec: &GateSpec) -> bool {
    match spec.origin {
        Origin::ShellGate => true,
        Origin::Hook => {
            spec.hook_id.as_deref() == Some(crate::mcp::error::GATE_POLICY_SUBJECT) && spec.tool == SHELL_WRITE
        }
        Origin::HookResult | Origin::KjVerb => false,
    }
}

/// The shell spec the council reads a submission under.
fn shell_spec(council: &CouncilConfig) -> Option<&CouncilSpec> {
    council.specs.iter().find(|s| s.case == CouncilCase::Shell)
}

/// Consult the council for one gate ask, or return `None` and leave the gate
/// as it is without one. `Err` is a fault reading the gate's own state, and
/// the caller refuses the submission as gate unavailable.
pub(crate) async fn consult(
    kernel: &crate::Kernel,
    caller: &KjCaller,
    spec: &GateSpec,
    config: &GateConfigLoad,
) -> Result<Option<CouncilVerdict>, String> {
    if !eligible(spec) {
        return Ok(None);
    }
    let Ok(config) = config else { return Ok(None) };
    let Some(council) = config.council() else { return Ok(None) };
    let (Some(context_id), Some(command)) = (caller.context_id, spec.exec_source.as_deref()) else {
        return Ok(None);
    };
    if spec.planned.is_empty() {
        return Ok(None);
    }
    let (context_type, cwd) = {
        let db = kernel.kernel_db().lock();
        let context_type = crate::kj::gate_policy::context_type_of(&db, Some(context_id));
        if !config.council_enabled_for(context_type.as_deref()) {
            return Ok(None);
        }
        let layers = Layers { config, context_type: context_type.as_deref() };
        let context = context_id.as_bytes().to_vec();
        let principal = caller.principal_id.as_bytes().to_vec();
        let policy = crate::kj::gate_policy::evaluate(db.conn_for_ledger(), spec, Some(&context), Some(&principal), layers)
            .map_err(|e| format!("the council could not read the gate rules: {e} (fail-closed — a ledger fault, not a decision)"))?;
        let undecided = policy
            .per_statement
            .iter()
            .any(|v| matches!(v, PolicyVerdict::Uncovered | PolicyVerdict::Ask(_)));
        if policy.verdict() != AskVerdict::Escalate || !undecided {
            return Ok(None);
        }
        let held = crate::kj::gate::ask_holding_submission(&db, spec, caller)
            .map_err(|e| format!("the council could not check for an earlier ask: {e} (fail-closed — a ledger fault, not a decision)"))?;
        if let Some(request_id) = held {
            tracing::info!(
                ask.held_by = %request_id,
                "the council is not consulted: an earlier ask still holds this submission"
            );
            return Ok(None);
        }
        let cwd = db
            .get_context_shell(context_id)
            .map_err(|e| format!("the council could not read the context's working directory: {e}"))?
            .and_then(|row| row.cwd);
        (context_type, cwd)
    };
    let running = crate::kj::gate::approved_run_unsettled(kernel, spec, caller)
        .map_err(|e| format!("the council {e} (fail-closed — a ledger fault, not a decision)"))?;
    if let Some(request_id) = running {
        tracing::info!(
            ask.held_by = %request_id,
            "the council is not consulted: the approval worker has not finished running this command"
        );
        return Ok(None);
    }
    let Some(shell) = shell_spec(council) else { return Ok(None) };
    let submission = Submission {
        command,
        planned: &spec.planned,
        context_type: context_type.as_deref(),
        cwd: cwd.as_deref(),
    };
    Ok(Some(decide(kernel, caller, submission, council, shell).await))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! verify()-consistent decision responses for tests.

    use kaijutsu_council::math::{self, Row, WeightSpec};
    use kaijutsu_council::wire::{DecisionRequest, DecisionResponse, PoolMethod, PoolWeights};

    pub(crate) const OPTIONS: [&str; 3] = ["allow", "ask", "report"];

    /// The identity the test server reports.
    pub(crate) fn identity() -> serde_json::Value {
        serde_json::json!({"model": "test-council", "weight_hash": "w1", "tokenizer_hash": "t1",
                           "template": "mk-letters-1:0123456789abcdef", "engine": "e1"})
    }

    /// An answer to `request` whose reads put `logprobs[i]` (allow, ask,
    /// report) on the verdict, with every derived number computed the way
    /// `math::verify` recomputes it.
    pub(crate) fn answer(request: &DecisionRequest, logprobs: &[[f64; 3]]) -> serde_json::Value {
        let pool = request.pool.clone().unwrap_or_default();
        let method = pool.method.unwrap_or(PoolMethod::Linear);
        let weights = pool.weights.unwrap_or(PoolWeights::Uniform);
        let rows: Vec<Row> = logprobs.iter().map(|lp| Row::from_logprobs(lp).unwrap()).collect();
        let spec = match weights {
            PoolWeights::Uniform => WeightSpec::Uniform,
            PoolWeights::Mass => WeightSpec::Mass,
            PoolWeights::Given => WeightSpec::Given(pool.values.clone().unwrap()),
        };
        let pooled = math::pool(&rows, method, &spec).unwrap();
        let contexts = request.contexts.clone().unwrap_or_default();
        assert_eq!(contexts.len(), rows.len(), "one read per context");
        let probs = |p: &[f64]| serde_json::json!({"allow": p[0], "ask": p[1], "report": p[2]});
        let reads: Vec<serde_json::Value> = contexts
            .iter()
            .zip(&rows)
            .zip(logprobs)
            .map(|((c, row), lp)| {
                serde_json::json!({
                    "context": c.id,
                    "snapshot": c.at.as_ref().map(|s| s.to_string()).unwrap_or_else(|| format!("snap:{}", "0".repeat(64))),
                    "answers": {"verdict": {
                        "type": "choice",
                        "choice": OPTIONS[math::argmax(&row.probs)],
                        "probabilities": probs(&row.probs),
                        "confidence": math::confidence(row.mass, &row.probs),
                        "logprobs": {"allow": lp[0], "ask": lp[1], "report": lp[2]},
                        "mass": row.mass,
                    }},
                    "rendered_sha256": "0".repeat(64),
                })
            })
            .collect();
        let mut id = identity();
        id["spec_id"] = serde_json::json!(request.spec_id);
        serde_json::json!({
            "model": "test-council",
            "answers": {"verdict": {
                "type": "choice",
                "choice": OPTIONS[math::argmax(&pooled.probs)],
                "probabilities": probs(&pooled.probs),
                "confidence": pooled.confidence(),
                "agree": pooled.agree,
                "spread": pooled.spread,
            }},
            "reads": reads,
            "pool": {"method": method, "weights": weights, "normalized": {"verdict": pooled.weights}},
            "signals": {"control_text": []},
            "identity": id,
            "queue_ms": 1.0,
            "ms": 4.0,
        })
    }

    pub(crate) fn decode(value: serde_json::Value) -> DecisionResponse {
        serde_json::from_value(value).expect("a test answer decodes")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{answer, decode};
    use super::*;
    use crate::kj::gate_policy::CouncilThreshold;

    const CTX_A: &str = "0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b11";
    const CTX_B: &str = "0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b12";

    fn spec_id() -> kaijutsu_council::wire::SpecId {
        kaijutsu_council::wire::SpecId::parse(format!("sha256:{}", "a".repeat(64))).unwrap()
    }

    fn council(allow_at: f64, mass_floor: f64, require_agree: bool) -> CouncilConfig {
        CouncilConfig {
            server: "http://127.0.0.1:1".into(),
            contexts: vec!["voice".into(), "system-rules".into()],
            pool_method: CouncilPoolMethod::LogLinear,
            pool_weights: CouncilPoolWeights::Mass,
            deadline_ms: 700,
            require_agree,
            specs: vec![CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell }],
            thresholds: vec![CouncilThreshold {
                spec: "shell-gate".into(),
                identity: CouncilIdentity {
                    weight_hash: "w1".into(),
                    engine: "e1".into(),
                    tokenizer_hash: "t1".into(),
                    template: "mk-letters-1:0123456789abcdef".into(),
                },
                allow_at,
                mass_floor,
            }],
        }
    }

    fn request() -> DecisionRequest {
        let snap = |c: char| kaijutsu_council::wire::SnapshotId::parse(format!("snap:{}", c.to_string().repeat(64))).unwrap();
        DecisionRequest {
            spec_id: Some(spec_id()),
            contexts: Some(vec![
                ContextRef { id: CTX_A.into(), at: Some(snap('1')) },
                ContextRef { id: CTX_B.into(), at: Some(snap('2')) },
            ]),
            state: Json::from("git status"),
            pool: Some(Pool { method: Some(PoolMethod::Loglinear), weights: Some(PoolWeights::Mass), values: None }),
            timeout_ms: Some(700),
            ..Default::default()
        }
    }

    /// Two confident reads of allow.
    const ALLOW: [[f64; 3]; 2] = [[-0.001, -8.0, -9.0], [-0.002, -7.5, -9.0]];

    fn classified(logprobs: &[[f64; 3]], council: &CouncilConfig) -> Classification {
        let req = request();
        classify(&req, &decode(answer(&req, logprobs)), council, "shell-gate")
    }

    #[test]
    fn the_test_answers_verify() {
        let req = request();
        kaijutsu_council::math::verify(&decode(answer(&req, &ALLOW)), &req).expect("consistent");
    }

    #[test]
    fn a_confident_agreed_allow_allows() {
        let c = classified(&ALLOW, &council(0.98, -0.05, true));
        assert_eq!(c.outcome, Outcome::Allow);
        assert!(c.verdict.unwrap().p("allow").unwrap() > 0.99);
        assert_eq!(c.threshold.unwrap().allow_at, 0.98);
    }

    /// The threshold is inclusive: p(allow) exactly at `allow_at` allows.
    #[test]
    fn p_allow_at_the_threshold_allows_and_under_a_higher_one_asks() {
        let p = classified(&ALLOW, &council(0.98, -0.05, true)).verdict.unwrap().p("allow").unwrap();
        assert_eq!(classified(&ALLOW, &council(p, -0.05, true)).outcome, Outcome::Allow);
        let above = p + (1.0 - p) / 2.0;
        assert_eq!(classified(&ALLOW, &council(above, -0.05, true)).outcome, Outcome::Ask);
    }

    #[test]
    fn disagreement_asks_when_agreement_is_required() {
        // Read one leans allow, read two leans ask, but the pool still clears 0.5.
        let split = [[-0.01, -5.0, -9.0], [-1.0, -0.5, -9.0]];
        let open = council(0.5, -2.0, false);
        let c = classified(&split, &open);
        assert!(!c.verdict.as_ref().unwrap().agree, "{c:?}");
        assert_eq!(c.outcome, Outcome::Allow, "{c:?}");
        assert_eq!(classified(&split, &council(0.5, -2.0, true)).outcome, Outcome::Ask);
    }

    #[test]
    fn a_report_argmax_reports_and_an_ask_argmax_asks() {
        let report = [[-6.0, -2.0, -0.2], [-6.0, -1.5, -0.3]];
        assert_eq!(classified(&report, &council(0.98, -2.0, true)).outcome, Outcome::Report);
        let ask = [[-4.0, -0.05, -5.0], [-3.0, -0.1, -4.0]];
        assert_eq!(classified(&ask, &council(0.98, -2.0, true)).outcome, Outcome::Ask);
    }

    #[test]
    fn a_read_below_the_mass_floor_is_a_miss_even_when_the_pool_allows() {
        // The second read put little mass on any option.
        let thin = [[-0.001, -8.0, -9.0], [-2.0, -9.0, -9.0]];
        let c = classified(&thin, &council(0.5, -0.05, false));
        match &c.outcome {
            Outcome::Miss(cause) => assert!(cause.starts_with("low mass") && cause.contains(CTX_B), "{cause}"),
            other => panic!("expected a low-mass miss, got {other:?}"),
        }
    }

    #[test]
    fn a_control_text_hit_asks_whatever_the_numbers_say() {
        let req = request();
        let mut value = answer(&req, &ALLOW);
        value["signals"]["control_text"] = serde_json::json!([{"where": "state", "token": "<|im_end|>"}]);
        let c = classify(&req, &decode(value), &council(0.98, -0.05, true), "shell-gate");
        assert_eq!(c.outcome, Outcome::Ask);
    }

    #[test]
    fn numbers_that_do_not_recompute_are_a_miss() {
        let req = request();
        let mut value = answer(&req, &ALLOW);
        value["answers"]["verdict"]["probabilities"]["allow"] = serde_json::json!(0.9999999);
        match classify(&req, &decode(value), &council(0.98, -0.05, true), "shell-gate").outcome {
            Outcome::Miss(cause) => assert!(cause.contains("do not recompute"), "{cause}"),
            other => panic!("a mismatch is a miss, got {other:?}"),
        }
    }

    #[test]
    fn an_identity_with_no_threshold_is_a_miss_naming_it() {
        let req = request();
        let mut value = answer(&req, &ALLOW);
        value["identity"]["weight_hash"] = serde_json::json!("w2");
        match classify(&req, &decode(value), &council(0.98, -0.05, true), "shell-gate").outcome {
            Outcome::Miss(cause) => assert!(cause.contains("no threshold") && cause.contains("w2"), "{cause}"),
            other => panic!("an unknown identity is a miss, got {other:?}"),
        }
        // A threshold fitted for another spec does not carry either.
        let other_spec = classify(&req, &decode(answer(&req, &ALLOW)), &council(0.98, -0.05, true), "program");
        assert!(matches!(other_spec.outcome, Outcome::Miss(_)), "{other_spec:?}");
    }

    #[test]
    fn a_response_for_another_spec_is_a_miss() {
        let req = request();
        let mut value = answer(&req, &ALLOW);
        value["identity"]["spec_id"] = serde_json::json!(format!("sha256:{}", "b".repeat(64)));
        assert!(matches!(
            classify(&req, &decode(value), &council(0.98, -0.05, true), "shell-gate").outcome,
            Outcome::Miss(_)
        ));
    }

    /// The council decides the shell gate and the gate-policy ask on
    /// `shell_write`, and nothing else: another hook's ask, the read-only
    /// shell's gate-policy ask, a result review, and a `kj` verb are not its.
    #[test]
    fn only_the_shell_gate_and_the_gate_policy_ask_on_shell_write_are_eligible() {
        let params = |tool: &str| crate::mcp::types::KernelCallParams {
            instance: crate::mcp::types::InstanceId::new("builtin.shell_write"),
            tool: tool.into(),
            arguments: serde_json::json!({"command": "touch x"}),
        };
        let policy = crate::mcp::error::GATE_POLICY_SUBJECT;
        assert!(eligible(&crate::kj::shell_gate::build_shell_gate_spec("touch x").unwrap()));
        assert!(eligible(&crate::kj::hook_gate::build_hook_gate_spec(policy, "d".into(), &params("shell_write"))));
        assert!(!eligible(&crate::kj::hook_gate::build_hook_gate_spec("scorer", "d".into(), &params("shell_write"))));
        assert!(!eligible(&crate::kj::hook_gate::build_hook_gate_spec(policy, "d".into(), &params("shell"))));
        let review = crate::kj::hook_gate::build_result_review_spec(
            policy, crate::mcp::McpHookPhase::PostCall, "d".into(), &params("shell_write"), "out",
        );
        assert!(!eligible(&review));
        let mut verb = crate::kj::shell_gate::build_shell_gate_spec("touch x").unwrap();
        verb.origin = Origin::KjVerb;
        assert!(!eligible(&verb));
    }

    #[test]
    fn a_spec_without_an_allow_verdict_cannot_decide() {
        let mut spec: kaijutsu_council::wire::Spec =
            serde_json::from_str(include_str!("../../../kaijutsu-council/tests/fixtures/spec.json")).unwrap();
        assert_eq!(spec_lacks_verdict(&spec), None);
        spec.questions.retain(|q| q.id() != VERDICT);
        assert!(spec_lacks_verdict(&spec).unwrap().contains("no `verdict` choice"));
    }

    #[test]
    fn failures_read_as_plain_causes() {
        assert_eq!(failure_cause(&CouncilError::Timeout, 700), "no answer within the 700 ms deadline");
        let status = CouncilError::Status { status: 503, error: None, retry_after: None, body: "busy".into() };
        assert!(failure_cause(&status, 700).contains("503"));
        let transport = CouncilError::Transport("connection refused".into());
        assert!(failure_cause(&transport, 700).contains("could not be reached"));
    }

    fn prepared(contexts: &[ContextId]) -> Prepared {
        let identity: kaijutsu_council::wire::ServerIdentity = serde_json::from_value(serde_json::json!({
            "model": "m", "weight_hash": "w1", "tokenizer_hash": "t1", "template": "x", "engine": "e1",
            "limits": {"context_tokens": 1, "state_bytes": 1, "contexts_per_decision": 8,
                       "choice_options": 8, "default_timeout_ms": 1000},
            "capabilities": []})).unwrap();
        let spec: kaijutsu_council::wire::Spec =
            serde_json::from_str(include_str!("../../../kaijutsu-council/tests/fixtures/spec.json")).unwrap();
        Prepared {
            client: kaijutsu_council::CouncilClient::new("http://127.0.0.1:1", Duration::from_millis(5)).unwrap(),
            identity,
            spec_id: kaijutsu_council::canon::spec_id(&spec).unwrap(),
            spec,
            contexts: contexts
                .iter()
                .enumerate()
                .map(|(i, id)| super::super::sync::PreparedContext {
                    label: format!("c{i}"),
                    context_id: *id,
                    head: kaijutsu_council::wire::SnapshotId::parse(format!("snap:{}", i.to_string().repeat(64))).unwrap(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_request_pins_each_context_to_its_prepared_head_and_carries_the_deadline() {
        let ids = [ContextId::new(), ContextId::new()];
        let p = prepared(&ids);
        let planned = kaish_kernel::plan_program("echo hi > out.txt; git push origin main").unwrap();
        let state = case_state(&Submission {
            command: "echo hi > out.txt; git push origin main",
            planned: &planned,
            context_type: Some("coder"),
            cwd: Some("/src"),
        });
        let req = decision_request(&p, &council(0.98, -0.05, true), state);
        req.validate().unwrap();
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body["spec_id"], serde_json::json!(p.spec_id.to_string()));
        assert_eq!(body["contexts"][0]["id"], serde_json::json!(ids[0].to_string()));
        assert_eq!(body["contexts"][0]["at"], serde_json::json!(p.contexts[0].head.to_string()));
        assert_eq!(body["contexts"][1]["at"], serde_json::json!(p.contexts[1].head.to_string()));
        assert_eq!(body["timeout_ms"], serde_json::json!(700));
        assert_eq!(body["pool"], serde_json::json!({"method": "loglinear", "weights": "mass"}));
        assert!(body.get("ask").is_none() && body.get("options").is_none(), "the gate never narrows: {body}");
        let text = serde_json::to_string(&req.state).unwrap();
        let order: Vec<usize> = ["\"command\"", "\"statements\"", "\"context_type\"", "\"cwd\""]
            .iter()
            .map(|k| text.find(k).unwrap_or_else(|| panic!("{k} missing from {text}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "members keep their order: {text}");
        let state: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(state["command"], "echo hi > out.txt; git push origin main");
        assert_eq!(state["statements"].as_array().unwrap().len(), 2);
        assert_eq!(state["statements"][1]["clauses"][0], "git push origin main");
        assert_eq!(state["context_type"], "coder");
        assert_eq!(state["cwd"], "/src");
    }

    #[test]
    fn a_404_naming_a_context_resends_it_and_one_naming_none_resends_all() {
        let ids = [ContextId::new(), ContextId::new()];
        let p = prepared(&ids);
        let named = CouncilError::Status {
            status: 404,
            error: Some(kaijutsu_council::wire::ErrorDetail {
                r#type: kaijutsu_council::wire::ErrorType::NotFound,
                message: format!("unknown context {}", ids[1]),
                param: None,
                head: None,
            }),
            retry_after: None,
            body: String::new(),
        };
        assert_eq!(contexts_to_resend(&named, &p), vec![ids[1]]);
        let bare = CouncilError::Status { status: 409, error: None, retry_after: None, body: String::new() };
        assert_eq!(contexts_to_resend(&bare, &p), ids.to_vec());
        let busy = CouncilError::Status { status: 503, error: None, retry_after: None, body: String::new() };
        assert!(contexts_to_resend(&busy, &p).is_empty());
    }
}
