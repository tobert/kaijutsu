//! The reviewer contexts a decision composes along the reviewer chain
//! (`docs/council.md`, "Council contexts are kaijutsu contexts").
//!
//! [`reviewer_context_chain`] starts at the submitting seat's reviewer, resolved the way
//! the gate resolves an ask's reviewer (`KernelDb::effective_approval_reviewer`),
//! and climbs the way `kj ledger escalate` does
//! (`KernelDb::responsible_character_above`, excluding the actor and every
//! character already on the chain) until it reaches the first live root
//! character. Each character on the chain contributes its council context,
//! labeled `council-<character>`:
//!
//! - a live root's council context **votes**: the gate adds it to the decision's
//!   contexts ([`ReviewerContextChain::decision_labels`]);
//! - a model character's council context **observes**: it is read under the
//!   `direction-check` spec after the gate decides (`super::observe`);
//! - a character with no live council context is **skipped**, and the skip is
//!   recorded; it is not a miss.

use kaijutsu_types::{ContextId, PrincipalId};

use super::projection::{SYSTEM_RULES, COUNCIL_CONTEXT_PREFIX};
use crate::kernel_db::KernelDb;
use crate::kj::gate_policy::{CouncilConfig, CouncilSpec};

/// A character on the chain with no `council-<character>` context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkippedReviewerContext {
    pub(crate) principal_id: PrincipalId,
    pub(crate) name: String,
}

impl SkippedReviewerContext {
    /// The ledger row for this skip.
    pub(crate) fn recorded(&self) -> approval_ledger::council_observation::CouncilReviewerSkip {
        approval_ledger::council_observation::CouncilReviewerSkip {
            principal_id: self.principal_id.as_bytes().to_vec(),
            character_name: self.name.clone(),
        }
    }
}

/// The reviewer contexts along one submission's reviewer chain, nearest reviewer first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewerContextChain {
    /// Live root characters' council labels; at most one, the chain's top.
    pub(crate) voting: Vec<String>,
    /// Model characters' council labels.
    pub(crate) observing: Vec<String>,
    pub(crate) skipped: Vec<SkippedReviewerContext>,
}

impl ReviewerContextChain {
    /// The labels a gate decision on `spec` reads: `[council] contexts`, the
    /// spec's own contexts, then each voting reviewer context those do not already hold.
    pub(crate) fn decision_labels(&self, council: &CouncilConfig, spec: &CouncilSpec) -> Vec<String> {
        let mut labels = council.contexts.clone();
        labels.extend(spec.contexts.iter().cloned());
        for label in &self.voting {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
        }
        labels
    }
}

/// The council context label for the character named `name`.
pub(crate) fn reviewer_label(name: &str) -> String {
    format!("{COUNCIL_CONTEXT_PREFIX}{name}")
}

/// Walk the reviewer chain above `actor` in `context` and collect each
/// character's council context. Empty when `[council] reviewer_contexts` is off.
/// `Err` when the reviewer cannot be resolved, the walk refuses (a retired
/// responsible character, a broken forest), or a character's council context label
/// would be the system rules context.
pub(crate) fn reviewer_context_chain(
    db: &KernelDb,
    council: &CouncilConfig,
    context: ContextId,
    actor: PrincipalId,
) -> Result<ReviewerContextChain, String> {
    let mut chain = ReviewerContextChain::default();
    if !council.reviewer_contexts {
        return Ok(chain);
    }
    let first = db
        .effective_approval_reviewer(context, actor)
        .map_err(|e| format!("the reviewer contexts need the seat's reviewer: {e}"))?
        .reviewer();
    let mut seen: Vec<PrincipalId> = Vec::new();
    let mut next = Some(first);
    while let Some(principal) = next {
        seen.push(principal);
        let sheet = db
            .get_character(principal)
            .map_err(|e| format!("the reviewer contexts could not read a reviewer's character sheet: {e}"))?
            .ok_or_else(|| format!("reviewer {} on the reviewer chain has no character sheet", principal.short()))?;
        let label = reviewer_label(&sheet.name);
        if label == SYSTEM_RULES {
            return Err(format!(
                "character '{}' on the reviewer chain would read {SYSTEM_RULES}, the system rules, as its \
                 council context; rename the character or turn [council] reviewer_contexts off",
                sheet.name
            ));
        }
        let held = db
            .find_context_by_label(&label)
            .map_err(|e| format!("the reviewer contexts could not look up \"{label}\": {e}"))?
            .is_some();
        let root = sheet.is_live_root();
        match (held, root) {
            (false, _) => chain.skipped.push(SkippedReviewerContext { principal_id: principal, name: sheet.name }),
            (true, true) => chain.voting.push(label),
            (true, false) => chain.observing.push(label),
        }
        if root {
            break;
        }
        let mut excluded = seen.clone();
        excluded.push(actor);
        next = db
            .responsible_character_above(context, &excluded)
            .map_err(|e| format!("the reviewer contexts could not walk the accountability forest: {e}"))?;
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::super::projection::fixtures::{character, live_context, live_context_with};
    use super::*;
    use crate::Kernel;

    const REVIEWER_CONTEXTS_ON: &str = r#"
[council]
server = "http://127.0.0.1:1"
contexts = ["council-system"]
reviewer_contexts = true
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 700

[[council.spec]]
name = "shell-gate"
case = "shell"
"#;

    fn config(text: &str) -> crate::kj::gate_policy::GateConfig {
        crate::kj::gate_policy::GateConfig::parse(text).expect("parses")
    }

    /// amy (root) plays her root context; banto plays a seat forked from it;
    /// coder plays a lane forked from banto's seat.
    struct Forest {
        kernel: Kernel,
        amy: PrincipalId,
        banto: PrincipalId,
        coder: PrincipalId,
        root: ContextId,
        lane: ContextId,
    }

    async fn forest() -> Forest {
        let kernel = Kernel::new_ephemeral("council-reviewer-contexts").await;
        let amy = character(&kernel, "amy", true);
        let banto = character(&kernel, "banto", false);
        let coder = character(&kernel, "coder", false);
        let root = live_context_with(&kernel, "amy", |r| r.played_by = Some(amy));
        let seat = live_context_with(&kernel, "banto", |r| {
            r.forked_from = Some(root);
            r.played_by = Some(banto);
            r.director_id = Some(amy);
        });
        let lane = live_context_with(&kernel, "lane-a", |r| {
            r.forked_from = Some(seat);
            r.played_by = Some(coder);
            r.director_id = Some(banto);
        });
        Forest { kernel, amy, banto, coder, root, lane }
    }

    impl Forest {
        fn chain(&self, text: &str, context: ContextId, actor: PrincipalId) -> Result<ReviewerContextChain, String> {
            reviewer_context_chain(&self.kernel.kernel_db().lock(), config(text).council().expect("[council]"), context, actor)
        }
    }

    #[tokio::test]
    async fn a_coder_under_banto_for_amy_votes_amy_and_observes_banto() {
        let f = forest().await;
        live_context(&f.kernel, "council-amy");
        live_context(&f.kernel, "council-banto");
        let chain = f.chain(REVIEWER_CONTEXTS_ON, f.lane, f.coder).unwrap();
        assert_eq!(chain.voting, ["council-amy"]);
        assert_eq!(chain.observing, ["council-banto"]);
        assert!(chain.skipped.is_empty());
        let council = config(REVIEWER_CONTEXTS_ON);
        let council = council.council().unwrap();
        assert_eq!(chain.decision_labels(council, &council.specs[0]), ["council-system", "council-amy"]);
        let coded = config(&REVIEWER_CONTEXTS_ON.replace("case = \"shell\"", "case = \"shell\"\ncontexts = [\"council-code\"]"));
        let coded = coded.council().unwrap();
        assert_eq!(
            chain.decision_labels(coded, &coded.specs[0]),
            ["council-system", "council-code", "council-amy"],
            "a spec's own contexts come after [council] contexts and before the reviewer contexts"
        );
    }

    #[tokio::test]
    async fn a_character_without_a_reviewer_context_is_skipped_not_missed() {
        let f = forest().await;
        live_context(&f.kernel, "council-amy");
        let chain = f.chain(REVIEWER_CONTEXTS_ON, f.lane, f.coder).unwrap();
        assert_eq!(chain.voting, ["council-amy"]);
        assert!(chain.observing.is_empty());
        assert_eq!(chain.skipped, [SkippedReviewerContext { principal_id: f.banto, name: "banto".into() }]);
        assert_eq!(chain.skipped[0].recorded().character_name, "banto");
    }

    #[tokio::test]
    async fn a_root_confirming_itself_reads_its_own_council_context() {
        let f = forest().await;
        live_context(&f.kernel, "council-amy");
        let chain = f.chain(REVIEWER_CONTEXTS_ON, f.root, f.amy).unwrap();
        assert_eq!(chain.voting, ["council-amy"]);
        assert!(chain.observing.is_empty() && chain.skipped.is_empty());
    }

    #[tokio::test]
    async fn the_walk_climbs_every_director_nearest_first_and_stops_at_the_root() {
        let f = forest().await;
        let lead = character(&f.kernel, "lead", false);
        let sub = live_context_with(&f.kernel, "lead-seat", |r| {
            r.forked_from = Some(f.lane);
            r.played_by = Some(lead);
            r.director_id = Some(f.coder);
        });
        let worker = character(&f.kernel, "worker", false);
        let leaf = live_context_with(&f.kernel, "leaf", |r| {
            r.forked_from = Some(sub);
            r.played_by = Some(worker);
        });
        for label in ["council-amy", "council-banto", "council-lead", "council-coder"] {
            live_context(&f.kernel, label);
        }
        let chain = f.chain(REVIEWER_CONTEXTS_ON, leaf, worker).unwrap();
        assert_eq!(chain.observing, ["council-lead", "council-coder", "council-banto"]);
        assert_eq!(chain.voting, ["council-amy"]);
    }

    #[tokio::test]
    async fn reviewer_contexts_off_reads_no_chain() {
        let f = forest().await;
        live_context(&f.kernel, "council-amy");
        live_context(&f.kernel, "council-banto");
        let off = REVIEWER_CONTEXTS_ON.replace("reviewer_contexts = true\n", "");
        assert_eq!(f.chain(&off, f.lane, f.coder).unwrap(), ReviewerContextChain::default());
    }

    #[tokio::test]
    async fn a_reviewer_context_already_listed_is_not_read_twice() {
        let f = forest().await;
        live_context(&f.kernel, "council-amy");
        let listed = REVIEWER_CONTEXTS_ON.replace(r#"contexts = ["council-system"]"#, r#"contexts = ["council-system", "council-amy"]"#);
        let chain = f.chain(&listed, f.lane, f.coder).unwrap();
        let council = config(&listed);
        let council = council.council().unwrap();
        assert_eq!(chain.decision_labels(council, &council.specs[0]), ["council-system", "council-amy"]);
    }

    #[tokio::test]
    async fn a_character_named_system_fails_loudly() {
        let f = forest().await;
        let system = character(&f.kernel, "system", false);
        let ctx = live_context_with(&f.kernel, "odd", |r| {
            r.played_by = Some(f.coder);
            r.reviewer_id = Some(system);
        });
        let error = f.chain(REVIEWER_CONTEXTS_ON, ctx, f.coder).unwrap_err();
        assert!(error.contains("council-system"), "{error}");
    }
}
