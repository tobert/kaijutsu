//! A model character's voice observes a gate decision (`docs/council.md`,
//! "Council contexts are kaijutsu contexts").
//!
//! After the gate decides, each observing voice the reviewer chain found
//! ([`super::voices::VoiceChain::observing`]) is read alone under the
//! `direction-check` spec, with the same case state the decision read. The
//! answer is verified and recorded with the decision
//! (`approval_ledger::council_observation`). It never enters the decision's
//! pool and never changes its outcome: the read names only the voice, and
//! nothing here writes a decision row.
//!
//! [`spawn_observe`] runs the reads off the hot path. Every failure, from
//! preparing the voice to writing the record, is the observation's own
//! outcome: a miss in the record, or an error in the log and in
//! [`Observed::record`]. Nothing reaches the gate.

use std::sync::Arc;
use std::time::{Duration, Instant};

use approval_ledger::council::{CouncilQuestion, CouncilServer};
use approval_ledger::council_observation::{CouncilObservationOutcome, NewCouncilObservation, insert_council_observation};
use kaijutsu_mk::Json;
use kaijutsu_mk::council::wire::{DecisionResponse, ReadAnswer, SpecQuestion};

use super::sync::Prepared;
use crate::kj::gate_policy::CouncilConfig;

/// The spec an observing voice is read under,
/// `/config/kernel/council/direction-check.json`.
pub(crate) const DIRECTION_CHECK: &str = "direction-check";

/// The `direction-check` question an observation's choice answers.
pub(crate) const FOLLOWS: &str = "follows";

/// What one observation found.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Seen {
    /// The read's top option for [`FOLLOWS`].
    Answered(String),
    /// No usable answer; the cause in plain words.
    Miss(String),
}

/// One observing voice's read and whether its record was written.
#[derive(Clone, Debug)]
pub(crate) struct Observed {
    pub(crate) label: String,
    pub(crate) seen: Seen,
    /// The observation id, or why the record could not be written.
    pub(crate) record: Result<Vec<u8>, String>,
}

impl Observed {
    /// One log line for this observation: an error when its record could
    /// not be written, a warning for a miss, and otherwise its choice.
    fn log(&self) {
        let label = &self.label;
        match (&self.record, &self.seen) {
            (Err(error), _) => tracing::error!(
                target: "kaijutsu::council",
                voice = %label,
                error = %error,
                "a council observation could not be recorded; the gate's decision stands"
            ),
            (Ok(_), Seen::Miss(cause)) => tracing::warn!(
                target: "kaijutsu::council",
                voice = %label,
                miss_cause = %cause,
                "a council voice gave no observation"
            ),
            (Ok(_), Seen::Answered(choice)) => tracing::info!(
                target: "kaijutsu::council",
                voice = %label,
                choice = %choice,
                "a council voice observed a decision"
            ),
        }
    }
}

/// Read each of `labels` under [`DIRECTION_CHECK`] on a task of its own, and
/// record each read against `decision_id`. `None` when there is nothing to
/// read. The task's result is for tests and logs; the gate never awaits it.
pub(crate) fn spawn_observe(
    kernel: Arc<crate::Kernel>,
    council: CouncilConfig,
    decision_id: Vec<u8>,
    labels: Vec<String>,
    state: Json,
) -> Option<tokio::task::JoinHandle<Vec<Observed>>> {
    if labels.is_empty() {
        return None;
    }
    Some(tokio::spawn(async move { observe_voices(&kernel, &council, &decision_id, &labels, &state).await }))
}

/// Read each of `labels` alone under [`DIRECTION_CHECK`], one after
/// another, and record each against `decision_id`.
pub(crate) async fn observe_voices(
    kernel: &crate::Kernel,
    council: &CouncilConfig,
    decision_id: &[u8],
    labels: &[String],
    state: &Json,
) -> Vec<Observed> {
    let mut observed = Vec::with_capacity(labels.len());
    for label in labels {
        let row = observe_one(kernel, council, decision_id, label, state).await;
        let seen = match (&row.outcome, &row.choice, &row.miss_cause) {
            (CouncilObservationOutcome::Answered, Some(choice), _) => Seen::Answered(choice.clone()),
            (_, _, Some(cause)) => Seen::Miss(cause.clone()),
            _ => unreachable!("observe_one sets a choice or a cause"),
        };
        let record = insert_council_observation(kernel.kernel_db().lock().conn_for_ledger(), &row)
            .map_err(|e| e.to_string());
        let one = Observed { label: label.clone(), seen, record };
        one.log();
        observed.push(one);
    }
    observed
}

fn empty_server() -> CouncilServer {
    CouncilServer {
        model: String::new(),
        weight_hash: String::new(),
        tokenizer_hash: String::new(),
        template: String::new(),
        engine: String::new(),
    }
}

/// The observation row for one voice. Every failure is a miss row.
async fn observe_one(
    kernel: &crate::Kernel,
    council: &CouncilConfig,
    decision_id: &[u8],
    label: &str,
    state: &Json,
) -> NewCouncilObservation {
    let started = Instant::now();
    let mut row = NewCouncilObservation {
        decision_id: decision_id.to_vec(),
        voice_label: label.to_string(),
        voice_context_id: None,
        spec_name: DIRECTION_CHECK.to_string(),
        spec_id: String::new(),
        server: empty_server(),
        outcome: CouncilObservationOutcome::Miss,
        choice: None,
        miss_cause: None,
        snapshot: None,
        expected_head: None,
        queue_ms: 0,
        ms: 0,
        questions: Vec::new(),
    };
    let miss = |mut row: NewCouncilObservation, cause: String, elapsed: Duration| {
        row.outcome = CouncilObservationOutcome::Miss;
        row.miss_cause = Some(cause);
        row.ms = elapsed.as_millis() as i64;
        row
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(council.deadline_ms);
    let labels = [label.to_string()];
    let prepared = match super::gate::prepare_within(kernel, council, DIRECTION_CHECK, &labels, None, deadline).await {
        Ok(prepared) => prepared,
        Err(cause) => return miss(row, cause, started.elapsed()),
    };
    let voice = &prepared.contexts[0];
    row.voice_context_id = Some(voice.context_id.as_bytes().to_vec());
    row.expected_head = Some(voice.head.to_string());
    row.spec_id = prepared.spec_id.to_string();
    row.server = CouncilServer {
        model: prepared.identity.model.clone(),
        weight_hash: prepared.identity.weight_hash.clone(),
        tokenizer_hash: prepared.identity.tokenizer_hash.clone(),
        template: prepared.identity.template.clone(),
        engine: prepared.identity.engine.clone(),
    };
    if let Some(cause) = spec_lacks_follows(&prepared) {
        return miss(row, cause, started.elapsed());
    }

    let request = super::gate::decision_request(&prepared, council, state.clone());
    let asked = Instant::now();
    let answer = super::gate::ask_within(kernel, &prepared, &request, council, deadline).await;
    let elapsed = asked.elapsed();
    let response = match answer {
        Ok(response) => response,
        Err(cause) => return miss(row, cause, elapsed),
    };
    row.server = CouncilServer {
        model: response.identity.model.clone(),
        weight_hash: response.identity.weight_hash.clone(),
        tokenizer_hash: response.identity.tokenizer_hash.clone(),
        template: response.identity.template.clone(),
        engine: response.identity.engine.clone(),
    };
    row.queue_ms = response.queue_ms.unwrap_or(0.0).round() as i64;
    match read_answer(&prepared, &request, &response) {
        Ok((snapshot, questions, choice)) => {
            row.outcome = CouncilObservationOutcome::Answered;
            row.choice = Some(choice);
            row.snapshot = snapshot;
            row.questions = questions;
            row.ms = elapsed.as_millis() as i64;
            row
        }
        Err(cause) => miss(row, cause, elapsed),
    }
}

/// Why the prepared spec cannot be observed under, when it cannot: an
/// observation reads a [`FOLLOWS`] choice.
fn spec_lacks_follows(prepared: &Prepared) -> Option<String> {
    match prepared.spec.questions.iter().find(|q| q.id() == FOLLOWS) {
        Some(SpecQuestion::Choice(_)) => None,
        Some(_) => Some(format!("spec {DIRECTION_CHECK} has a `{FOLLOWS}` question that is not a choice")),
        None => Some(format!("spec {DIRECTION_CHECK} has no `{FOLLOWS}` choice")),
    }
}

/// The verified read of the one voice: its snapshot, every question it
/// answered, and its top option for [`FOLLOWS`].
fn read_answer(
    prepared: &Prepared,
    request: &kaijutsu_mk::council::wire::DecisionRequest,
    response: &DecisionResponse,
) -> Result<(Option<String>, Vec<CouncilQuestion>, String), String> {
    kaijutsu_mk::council::math::verify(response, request)
        .map_err(|mismatch| format!("the answer's numbers do not recompute ({mismatch})"))?;
    if let (Some(read), Some(asked)) = (&response.identity.spec_id, &request.spec_id)
        && read != asked
    {
        return Err(format!("the server read spec {read}, not the spec asked for, {asked}"));
    }
    let voice = prepared.contexts[0].context_id.to_string();
    let [read] = response.reads.as_slice() else {
        return Err(format!("the answer has {} reads, not the one voice asked for", response.reads.len()));
    };
    if read.context.as_deref() != Some(voice.as_str()) {
        return Err(format!(
            "the answer read {}, not the voice asked for, {voice}",
            read.context.as_deref().unwrap_or("the spec alone")
        ));
    }
    let choice = match read.answers.get(FOLLOWS) {
        Some(ReadAnswer::Choice(c)) => c.choice.clone(),
        _ => return Err(format!("the answer has no `{FOLLOWS}` choice")),
    };
    let questions = read.answers.iter().map(|(id, answer)| super::gate::read_question(id, answer)).collect();
    Ok((read.snapshot.as_ref().map(|s| s.to_string()), questions, choice))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! verify()-consistent `direction-check` answers for tests.

    use kaijutsu_mk::council::math::{self, Row, WeightSpec};
    use kaijutsu_mk::council::wire::{DecisionRequest, PoolMethod, PoolWeights};

    pub(crate) const OPTIONS: [&str; 3] = ["follows", "strays", "unclear"];

    /// An answer to `request` whose one read puts `logprobs` (follows,
    /// strays, unclear) on the `follows` question.
    pub(crate) fn follows_answer(request: &DecisionRequest, logprobs: [f64; 3]) -> serde_json::Value {
        let pool = request.pool.clone().unwrap_or_default();
        let method = pool.method.unwrap_or(PoolMethod::Linear);
        let weights = pool.weights.unwrap_or(PoolWeights::Uniform);
        let row = Row::from_logprobs(&logprobs).unwrap();
        let spec = match weights {
            PoolWeights::Uniform => WeightSpec::Uniform,
            PoolWeights::Mass => WeightSpec::Mass,
            PoolWeights::Given => WeightSpec::Given(pool.values.clone().unwrap()),
        };
        let pooled = math::pool(std::slice::from_ref(&row), method, &spec).unwrap();
        let probs = |p: &[f64]| serde_json::json!({"follows": p[0], "strays": p[1], "unclear": p[2]});
        let context = &request.contexts.as_ref().unwrap()[0];
        serde_json::json!({
            "model": "test-council",
            "answers": {"follows": {
                "type": "choice",
                "choice": OPTIONS[math::argmax(&pooled.probs)],
                "probabilities": probs(&pooled.probs),
                "confidence": pooled.confidence(),
                "agree": pooled.agree,
                "spread": pooled.spread,
            }},
            "reads": [{
                "context": context.id,
                "snapshot": context.at,
                "answers": {"follows": {
                    "type": "choice",
                    "choice": OPTIONS[math::argmax(&row.probs)],
                    "probabilities": probs(&row.probs),
                    "confidence": math::confidence(row.mass, &row.probs),
                    "logprobs": {"follows": logprobs[0], "strays": logprobs[1], "unclear": logprobs[2]},
                    "mass": row.mass,
                }},
                "rendered_sha256": "0".repeat(64),
            }],
            "pool": {"method": method, "weights": weights, "normalized": {"follows": pooled.weights}},
            "signals": {"control_text": []},
            "identity": {"model": "test-council", "weight_hash": "w1", "tokenizer_hash": "t1",
                         "template": "mk-letters-1:0", "engine": "e1", "spec_id": request.spec_id},
            "queue_ms": 1.0,
            "ms": 4.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use approval_ledger::council::{CouncilOutcome, NewCouncilDecision, insert_council_decision, load_council_decision};
    use approval_ledger::council_observation::list_council_observations_for_decision;
    use kaijutsu_mk::council::wire::DecisionRequest;

    use super::super::projection::fixtures::{append_dialogue, live_context};
    use super::super::sync::mock::{Mock, reply, serve};
    use super::*;
    use crate::Kernel;
    use crate::kj::gate_policy::{CouncilCase, CouncilPoolMethod, CouncilPoolWeights, CouncilSpec};
    use crate::vfs::LocalBackend;

    fn answer(request: &DecisionRequest, logprobs: [f64; 3]) -> serde_json::Value {
        super::test_support::follows_answer(request, logprobs)
    }

    struct Rig {
        kernel: Arc<Kernel>,
        mock: Mock,
        council: CouncilConfig,
        decision: Vec<u8>,
        banto: kaijutsu_types::ContextId,
        _config: tempfile::TempDir,
    }

    async fn rig() -> Rig {
        let kernel = Arc::new(Kernel::new_ephemeral("council-observe").await);
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config.path().join("council")).unwrap();
        std::fs::write(config.path().join("council/shell-gate.json"), crate::config_seed::DEFAULT_COUNCIL_SHELL_GATE).unwrap();
        std::fs::write(
            config.path().join("council/direction-check.json"),
            crate::config_seed::DEFAULT_COUNCIL_DIRECTION_CHECK,
        )
        .unwrap();
        kernel.vfs().mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(config.path())).await;
        for label in ["council-system", "council-amy"] {
            let ctx = live_context(&kernel, label);
            append_dialogue(&kernel, ctx, &["never force push"]);
        }
        let banto = live_context(&kernel, "council-banto");
        append_dialogue(&kernel, banto, &["only touch the parser crate"]);
        let mock = serve().await;
        mock.on_decide(|body| {
            let request: DecisionRequest = serde_json::from_str(body).unwrap();
            reply(200, answer(&request, [-2.0, -0.2, -3.0]).to_string())
        });
        let council = CouncilConfig {
            server: mock.base.clone(),
            contexts: vec!["council-system".into(), "council-amy".into()],
            pool_method: CouncilPoolMethod::LogLinear,
            pool_weights: CouncilPoolWeights::Mass,
            deadline_ms: 5000,
            require_agree: true,
            voices: false,
            house_rules: false,
            house_rules_tokens: crate::kj::gate_policy::DEFAULT_HOUSE_RULES_TOKENS,
            mode: crate::kj::gate_policy::CouncilMode::Gatekeeper,
            bump_limit: Some(crate::kj::gate_policy::DEFAULT_BUMP_LIMIT),
            escalate: None,
            specs: vec![CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell, contexts: Vec::new(), require_agree: None }],
            thresholds: vec![],
        };
        let decision = insert_council_decision(
            kernel.kernel_db().lock().conn_for_ledger(),
            &NewCouncilDecision {
                request_id: None,
                context_id: vec![1],
                principal_id: vec![2],
                submission_digest: "sha256:d".into(),
                spec_id: "sha256:a".into(),
                spec_name: "shell-gate".into(),
                server: CouncilServer {
                    model: "test-council".into(),
                    weight_hash: "w1".into(),
                    tokenizer_hash: "t1".into(),
                    template: "mk-letters-1:0".into(),
                    engine: "e1".into(),
                },
                pool_method: "loglinear".into(),
                pool_weights: "mass".into(),
                threshold: None,
                deadline_ms: 700,
                outcome: CouncilOutcome::Ask,
                miss_cause: None,
                agreement: None,
                queue_ms: 0,
                ms: 3,
                reads: vec![],
                pooled: vec![],
                house_rules_head: None,
                bump_flavor: None,
                seat_bump: None,
                control_text: vec![],
            },
        )
        .unwrap();
        Rig { kernel, mock, council, decision, banto, _config: config }
    }

    fn state() -> Json {
        serde_json::from_str(r#"{"command": "git push --force", "statements": []}"#).unwrap()
    }

    impl Rig {
        async fn observe(&self, labels: &[&str]) -> Vec<Observed> {
            let labels: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
            spawn_observe(self.kernel.clone(), self.council.clone(), self.decision.clone(), labels, state())
                .expect("a task")
                .await
                .unwrap()
        }

        fn recorded(&self) -> Vec<approval_ledger::council_observation::CouncilObservation> {
            list_council_observations_for_decision(self.kernel.kernel_db().lock().conn_for_ledger(), &self.decision)
                .unwrap()
        }
    }

    #[tokio::test]
    async fn a_voice_is_read_alone_and_recorded_without_touching_the_decision() {
        let r = rig().await;
        let before = load_council_decision(r.kernel.kernel_db().lock().conn_for_ledger(), &r.decision).unwrap();
        let observed = r.observe(&["council-banto"]).await;
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].seen, Seen::Answered("strays".into()));
        let id = observed[0].record.clone().expect("recorded");

        let decisions = r.mock.bodies("/council/v1/decisions");
        assert_eq!(decisions.len(), 1);
        let contexts = decisions[0]["contexts"].as_array().unwrap();
        assert_eq!(contexts.len(), 1, "only the voice is read, never the decision's pool");
        assert_eq!(contexts[0]["id"], r.banto.to_string());
        assert_eq!(decisions[0]["state"]["command"], "git push --force");
        let specs = r.mock.bodies("/council/v1/specs");
        assert_eq!(specs.last().unwrap()["name"], DIRECTION_CHECK);

        let rows = r.recorded();
        assert_eq!(rows.len(), 1);
        let o = &rows[0].observation;
        assert_eq!(rows[0].observation_id, id);
        assert_eq!(o.voice_label, "council-banto");
        assert_eq!(o.voice_context_id.as_deref(), Some(r.banto.as_bytes().as_slice()));
        assert_eq!(o.choice.as_deref(), Some("strays"));
        assert_eq!(o.server.weight_hash, "w1");
        assert_eq!(o.questions.len(), 1);
        let options: Vec<(&str, f64)> = o.questions[0].options.iter().map(|x| (x.option.as_str(), x.logprob)).collect();
        assert_eq!(options, [("follows", -2.0), ("strays", -0.2), ("unclear", -3.0)]);
        let total: f64 = o.questions[0].options.iter().map(|x| x.probability).sum();
        assert!((total - 1.0).abs() < 1e-9, "probabilities renormalize over the options: {total}");

        let after = load_council_decision(r.kernel.kernel_db().lock().conn_for_ledger(), &r.decision).unwrap();
        assert_eq!(before, after, "the decision's rows are unchanged");
    }

    #[tokio::test]
    async fn a_server_failure_is_the_observations_own_miss() {
        let r = rig().await;
        r.mock.on_decide(|_| reply(500, "boom"));
        let observed = r.observe(&["council-banto"]).await;
        let Seen::Miss(cause) = &observed[0].seen else { panic!("a miss: {observed:?}") };
        assert!(cause.contains("500"), "{cause}");
        assert!(observed[0].record.is_ok());
        assert_eq!(r.recorded()[0].observation.outcome, CouncilObservationOutcome::Miss);
    }

    #[tokio::test]
    async fn an_answer_that_does_not_verify_is_a_miss() {
        let r = rig().await;
        r.mock.on_decide(|body| {
            let request: DecisionRequest = serde_json::from_str(body).unwrap();
            let mut a = answer(&request, [-2.0, -0.2, -3.0]);
            a["answers"]["follows"]["probabilities"]["strays"] = serde_json::json!(0.5);
            reply(200, a.to_string())
        });
        let observed = r.observe(&["council-banto"]).await;
        let Seen::Miss(cause) = &observed[0].seen else { panic!("a miss: {observed:?}") };
        assert!(cause.contains("recompute"), "{cause}");
    }

    #[tokio::test]
    async fn a_voice_gone_by_read_time_is_a_miss_without_a_seat() {
        let r = rig().await;
        let observed = r.observe(&["council-ghost"]).await;
        let Seen::Miss(cause) = &observed[0].seen else { panic!("a miss") };
        assert!(cause.contains("council-ghost"), "{cause}");
        assert_eq!(r.recorded()[0].observation.voice_context_id, None);
        assert!(r.mock.bodies("/council/v1/decisions").is_empty());
    }

    #[tokio::test]
    async fn a_record_that_cannot_be_written_is_reported_not_raised() {
        let mut r = rig().await;
        r.decision = vec![0; 16];
        let observed = r.observe(&["council-banto"]).await;
        assert_eq!(observed[0].seen, Seen::Answered("strays".into()));
        let error = observed[0].record.clone().unwrap_err();
        assert!(error.contains("no council decision"), "{error}");
    }

    #[tokio::test]
    async fn nothing_to_observe_spawns_nothing() {
        let r = rig().await;
        assert!(spawn_observe(r.kernel.clone(), r.council.clone(), r.decision.clone(), vec![], state()).is_none());
    }
}
