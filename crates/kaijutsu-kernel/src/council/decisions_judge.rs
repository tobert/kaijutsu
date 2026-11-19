//! The decisions judge: a hosted Decisions API on a context's `gate` slot
//! that judges a shell submission the gate's static rules escalate
//! (`docs/approval-identity.md`, "The guardian").
//!
//! It reads what the guardian reads ([`super::guardian::evidence`]) as the
//! decision's state, with the guardian's policy as the question's
//! instructions, and answers one choice question, `verdict`, with a
//! probability for each of `allow`, `ask`, and `report`. [`classify`] turns
//! them into the gate's outcome. Every failure is a miss with its cause, and
//! a miss never allows.

use super::gate::Outcome;
use super::guardian::{Answer, JudgeKind, Judged, ShownProgram};
use crate::llm::decisions::{Choice, ChoiceAnswer};

/// The name a decisions judge's decision is recorded under, as a council
/// spec's would be.
pub(crate) const SPEC: &str = "decisions";

/// The choice question's id.
pub(crate) const QUESTION: &str = "verdict";

/// How the policy's `outcome` reads as the `verdict` options, appended to
/// the policy so the question cannot drift from what [`classify`] reads.
pub(crate) const VERDICT_MAPPING: &str = "# Verdict
Choose the verdict for the planned action. It stands for the policy's `outcome`: `allow` when the policy allows it; `report` when it denies it at `critical` risk; `ask` for any other deny.";

/// The question: the policy and [`VERDICT_MAPPING`] as its instructions, and
/// the three verdicts with plain meanings.
pub(crate) fn question(policy: &str) -> Choice {
    Choice::new(
        format!("{}\n\n{VERDICT_MAPPING}", policy.trim_end()),
        [
            ("allow", "The policy allows it: run it without asking anyone."),
            ("ask", "The policy denies it below critical risk: a person decides whether it runs."),
            ("report", "Critical risk, such as sending secrets away or major irreversible destruction: stop and report it."),
        ],
    )
}

/// The probability of each verdict.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Probabilities {
    pub(crate) allow: f64,
    pub(crate) ask: f64,
    pub(crate) report: f64,
}

impl Probabilities {
    /// The answer's probabilities. `Err` names a verdict it lacks.
    pub(crate) fn of(answer: &ChoiceAnswer) -> Result<Self, String> {
        let p = |option: &str| {
            answer.p(option).ok_or_else(|| format!("the decisions answer has no probability for {option:?}"))
        };
        Ok(Self { allow: p("allow")?, ask: p("ask")?, report: p("report")? })
    }

    /// The probabilities as the ask and the log show them.
    pub(crate) fn summary(&self, allow_at: f64) -> String {
        format!(
            "p(allow) {:.3}, p(ask) {:.3}, p(report) {:.3}; allowing requires a choice of allow and p(allow) >= {allow_at}",
            self.allow, self.ask, self.report
        )
    }

    /// The probabilities as the ledger's pooled rows, under [`QUESTION`].
    pub(crate) fn recorded(&self) -> Vec<approval_ledger::council::CouncilPooled> {
        [("allow", self.allow), ("ask", self.ask), ("report", self.report)]
            .into_iter()
            .map(|(option, probability)| approval_ledger::council::CouncilPooled {
                question_id: QUESTION.to_string(),
                option: option.to_string(),
                probability,
            })
            .collect()
    }
}

/// Report when `report` is the most likely verdict, ties included.
/// Otherwise allow only when the judge chose `allow` and p(allow) meets
/// the threshold. Ask in all other cases.
fn classify(choice: &str, p: Probabilities, allow_at: f64) -> Outcome {
    if p.report >= p.allow && p.report >= p.ask {
        Outcome::Report
    } else if choice == "allow" && p.allow >= allow_at {
        Outcome::Allow
    } else {
        Outcome::Ask
    }
}

/// Ask the decisions judge on `bound` (the context's `gate` slot) about the
/// planned `action`. Every failure is a miss with its cause: a tripped
/// missing evidence, a slot whose backend is not a decisions client, a
/// failed request, or an answer that does not match the question. Runs with
/// no database lock held.
pub(crate) async fn judge(
    kernel: &crate::Kernel,
    bound: &crate::model_resolution::BoundSlot,
    config: &crate::kj::gate_policy::GuardianConfig,
    context: kaijutsu_types::ContextId,
    action: &serde_json::Value,
    programs: &[ShownProgram],
) -> Judged {
    let started = std::time::Instant::now();
    let mut judged = Judged::begin(JudgeKind::Decisions, bound, config);
    let evidence = match super::guardian::evidence(kernel, config, JudgeKind::Decisions, context, action, programs).await {
        Ok(evidence) => evidence,
        Err(cause) => return judged.missed(started, cause),
    };
    let question = question(&evidence.policy);
    judged.policy_digest = super::gate::text_digest(&serde_json::json!([question.instructions, question.options]).to_string());
    let provider = kernel.llm().read().await.get(&bound.backend);
    let Some(client) = provider.as_deref().and_then(crate::llm::Provider::decisions) else {
        let cause = format!("backend {} has no decisions client in the kernel's registry", bound.backend);
        return judged.missed(started, cause);
    };
    let deadline = std::time::Duration::from_millis(config.deadline_ms);
    let state = serde_json::Value::String(evidence.prompt);
    let questions = std::collections::BTreeMap::from([(QUESTION.to_string(), question)]);
    let response = match client.decide(&bound.slot.model, state, questions, deadline).await {
        Ok(response) => response,
        Err(e) => {
            let cause = format!("{} failed: {e}", judged.name());
            return judged.missed(started, cause);
        }
    };
    let usage = &response.usage;
    if let Err(e) = crate::runtime::llm_stream::record_turn_spend(
        kernel.kernel_db(), None, client.provider_name(), &bound.slot.model,
        Some(usage.input_tokens), Some(usage.output_tokens), &None,
    ) {
        tracing::error!(target: "kaijutsu::council", error = %e, "the decisions judge's spend could not be recorded");
    }
    let Some(answer) = response.answers.get(QUESTION) else {
        let cause = format!("{} failed: the reply has no verdict", judged.name());
        return judged.missed(started, cause);
    };
    let p = match Probabilities::of(answer) {
        Ok(p) => p,
        Err(cause) => {
            let cause = format!("{} failed: {cause}", judged.name());
            return judged.missed(started, cause);
        }
    };
    judged.outcome = classify(&answer.choice, p, config.allow_at);
    judged.answer = Some(Answer::Choice { p, allow_at: config.allow_at });
    judged.elapsed = started.elapsed();
    judged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(allow: f64, ask: f64, report: f64) -> Probabilities {
        Probabilities { allow, ask, report }
    }

    #[test]
    fn allow_at_the_threshold_allows_and_under_it_asks() {
        assert_eq!(classify("allow", p(0.9, 0.08, 0.02), 0.9), Outcome::Allow);
        assert_eq!(classify("allow", p(0.97, 0.02, 0.01), 0.9), Outcome::Allow);
        assert_eq!(classify("allow", p(0.89, 0.1, 0.01), 0.9), Outcome::Ask, "the most likely verdict, under the threshold");
        assert_eq!(classify("allow", p(0.95, 0.04, 0.01), 0.98), Outcome::Ask);
    }

    #[test]
    fn a_report_argmax_reports_whatever_the_threshold() {
        assert_eq!(classify("report", p(0.1, 0.3, 0.6), 0.9), Outcome::Report);
        assert_eq!(classify("report", p(0.35, 0.25, 0.4), 0.3), Outcome::Report, "report wins over a low allow_at");
        assert_eq!(classify("allow", p(0.4, 0.2, 0.4), 0.3), Outcome::Report, "a tie with report reports");
    }

    #[test]
    fn an_ask_argmax_asks_even_above_the_allow_threshold() {
        assert_eq!(classify("ask", p(0.4, 0.55, 0.05), 0.3), Outcome::Ask);
        assert_eq!(classify("ask", p(0.48, 0.48, 0.04), 0.3), Outcome::Ask);
        assert_eq!(classify("allow", p(0.48, 0.48, 0.04), 0.3), Outcome::Allow);
    }

    #[test]
    fn the_probabilities_read_from_an_answer() {
        let answer: ChoiceAnswer = serde_json::from_value(serde_json::json!({
            "choice": "ask", "probabilities": {"allow": 0.25, "ask": 0.7, "report": 0.05}
        }))
        .unwrap();
        assert_eq!(Probabilities::of(&answer), Ok(p(0.25, 0.7, 0.05)));
        let short: ChoiceAnswer =
            serde_json::from_value(serde_json::json!({"choice": "ask", "probabilities": {"allow": 0.3, "ask": 0.7}})).unwrap();
        assert!(Probabilities::of(&short).unwrap_err().contains("\"report\""));
    }

    #[test]
    fn the_question_carries_the_policy_and_three_verdicts() {
        let q = question("Judge it.\n");
        assert!(q.instructions.starts_with("Judge it.\n\n# Verdict"), "{}", q.instructions);
        assert_eq!(q.options.keys().map(String::as_str).collect::<Vec<_>>(), ["allow", "ask", "report"]);
    }
}
