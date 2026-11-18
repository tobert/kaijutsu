//! The guardian: a chat model on a context's `gate` slot that judges a
//! shell submission the gate's static rules escalate
//! (`docs/approval-identity.md`, "The guardian").
//!
//! The guardian reads a policy (`/config/kernel/council/guardian.md`), the
//! planned action, the workspace's `AGENTS.md`, and a bounded tail of the
//! context's transcript, and answers one JSON object: a verdict, a risk
//! level, the user's authorization, and a rationale. The gate acts on it as
//! it acts on a council decision ([`Assessment::outcome`]). Every failure is
//! a miss with its cause, and the gate then asks as it would without a
//! guardian.
//!
//! A context whose guardian denies 3 submissions in a row, or 10 of its last
//! 50, is no longer judged: each later submission is a miss naming the
//! [`Breaker`], so it asks the senpai.

use std::collections::{HashMap, VecDeque};

use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, Role, Status};
use serde::Deserialize;

use super::gate::Outcome;

/// The name a guardian decision is recorded under, as a council spec's
/// would be.
pub(crate) const SPEC: &str = "guardian";

/// The policy file, under `/config/kernel/`.
pub(crate) const POLICY_PATH: &str = "council/guardian.md";

/// The answer format, appended to the policy so the policy file cannot drift
/// from what [`parse_assessment`] accepts.
pub(crate) const ANSWER_FORMAT: &str = r#"# Answer format
Answer with one JSON object and nothing else: no code fence, and no text before or after it. Every field is required.

{"outcome": "allow" | "deny", "risk_level": "low" | "medium" | "high" | "critical", "user_authorization": "unknown" | "low" | "medium" | "high", "rationale": "<one sentence>"}"#;

/// The guardian's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Risk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Authorization {
    Unknown,
    Low,
    Medium,
    High,
}

impl Verdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

impl Risk {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

impl Authorization {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// The guardian's answer to one submission.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Assessment {
    pub(crate) outcome: Verdict,
    pub(crate) risk_level: Risk,
    pub(crate) user_authorization: Authorization,
    pub(crate) rationale: String,
}

/// Bytes of an unparseable answer a miss cause quotes.
const QUOTED_ANSWER: usize = 200;

/// Parse the guardian's answer: one JSON object with every field of
/// [`ANSWER_FORMAT`] and nothing around it but whitespace. `Err` says what is
/// wrong and quotes the start of the answer.
pub(crate) fn parse_assessment(text: &str) -> Result<Assessment, String> {
    let quoted = || {
        let trimmed = text.trim();
        match trimmed.len() > QUOTED_ANSWER {
            true => format!("{:?}...", head(trimmed, QUOTED_ANSWER)),
            false => format!("{trimmed:?}"),
        }
    };
    let assessment: Assessment = serde_json::from_str(text.trim())
        .map_err(|e| format!("the guardian's answer is not the JSON object the policy asks for ({e}): {}", quoted()))?;
    if assessment.rationale.trim().is_empty() {
        return Err(format!("the guardian's answer has an empty rationale: {}", quoted()));
    }
    Ok(assessment)
}

impl Assessment {
    /// What the gate does with this answer: an allow runs the submission; a
    /// deny at `critical` risk reports (an ask, and an autonomous context is
    /// stopped); any other deny asks. An allow at `critical` risk
    /// contradicts the policy, so it asks.
    pub(crate) fn outcome(&self) -> Outcome {
        match (self.outcome, self.risk_level) {
            (Verdict::Allow, Risk::Critical) => Outcome::Ask,
            (Verdict::Allow, _) => Outcome::Allow,
            (Verdict::Deny, Risk::Critical) => Outcome::Report,
            (Verdict::Deny, _) => Outcome::Ask,
        }
    }

    /// The answer as the ask and the log show it.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} at {} risk, user authorization {}: {}",
            self.outcome.as_str(),
            self.risk_level.as_str(),
            self.user_authorization.as_str(),
            self.rationale.trim()
        )
    }

    pub(crate) fn recorded(&self) -> approval_ledger::council::CouncilAssessment {
        approval_ledger::council::CouncilAssessment {
            verdict: self.outcome.as_str().to_string(),
            risk_level: self.risk_level.as_str().to_string(),
            user_authorization: self.user_authorization.as_str().to_string(),
            rationale: self.rationale.trim().to_string(),
        }
    }
}

/// Denials in a row that stop the guardian judging a context.
pub(crate) const MAX_CONSECUTIVE_DENIALS: u32 = 3;
/// Denials among the last [`DENIAL_WINDOW`] answers that stop it.
pub(crate) const MAX_RECENT_DENIALS: usize = 10;
pub(crate) const DENIAL_WINDOW: usize = 50;

/// One context's guardian answers, for the circuit breaker: once it trips,
/// the guardian judges nothing more in that context until the kernel
/// restarts, and each submission asks the senpai.
#[derive(Clone, Debug, Default)]
pub(crate) struct Breaker {
    consecutive: u32,
    recent: VecDeque<bool>,
    tripped: Option<String>,
}

impl Breaker {
    /// Count one answer. Returns the cause when this answer trips the
    /// breaker.
    pub(crate) fn record(&mut self, verdict: Verdict) -> Option<String> {
        let denied = verdict == Verdict::Deny;
        self.consecutive = if denied { self.consecutive.saturating_add(1) } else { 0 };
        self.recent.push_back(denied);
        if self.recent.len() > DENIAL_WINDOW {
            self.recent.pop_front();
        }
        if self.tripped.is_some() {
            return None;
        }
        let recent = self.recent.iter().filter(|d| **d).count();
        let cause = if self.consecutive >= MAX_CONSECUTIVE_DENIALS {
            format!("it denied {} submissions in a row", self.consecutive)
        } else if recent >= MAX_RECENT_DENIALS {
            format!("it denied {recent} of the last {} submissions", self.recent.len())
        } else {
            return None;
        };
        self.tripped = Some(cause.clone());
        Some(cause)
    }

    /// Why the guardian no longer judges this context, once tripped.
    pub(crate) fn tripped(&self) -> Option<&str> {
        self.tripped.as_deref()
    }
}

/// Every context's [`Breaker`]. Kept in memory: a kernel restart resets them.
#[derive(Debug, Default)]
pub(crate) struct Breakers(parking_lot::Mutex<HashMap<ContextId, Breaker>>);

impl Breakers {
    pub(crate) fn tripped(&self, context: ContextId) -> Option<String> {
        self.0.lock().get(&context).and_then(|b| b.tripped().map(str::to_owned))
    }

    pub(crate) fn record(&self, context: ContextId, verdict: Verdict) -> Option<String> {
        self.0.lock().entry(context).or_default().record(verdict)
    }
}

/// Bytes one block shows in the transcript.
const BLOCK_BYTES: usize = 2048;

/// The tail of a context's transcript for the guardian, oldest first, within
/// `budget` bytes. Each finished, unexcluded block is one entry, labeled by
/// who wrote it: `[user]` (trusted), `[assistant]`, `[tool call <name>]`,
/// `[tool result]`, and `[drift]` (untrusted evidence). Thinking, system
/// instructions, and files stay out. A block longer than 2048 bytes is cut,
/// and blocks before the budget are counted in a leading marker.
pub(crate) fn transcript(blocks: &[BlockSnapshot], budget: usize) -> String {
    let entries: Vec<String> = blocks.iter().filter_map(entry).collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for entry in entries.iter().rev() {
        if used + entry.len() > budget {
            break;
        }
        used += entry.len() + 1;
        kept.push(entry);
    }
    kept.reverse();
    let dropped = entries.len() - kept.len();
    let mut text = String::new();
    if dropped > 0 {
        text.push_str(&format!("<truncated {dropped} earlier blocks />\n"));
    }
    text.push_str(&kept.join("\n"));
    text
}

fn entry(b: &BlockSnapshot) -> Option<String> {
    if b.excluded || b.ephemeral || b.status != Status::Done {
        return None;
    }
    let label = match (b.kind, b.role) {
        (BlockKind::Text, Role::User) => "[user]".to_string(),
        (BlockKind::Text, Role::Model) => "[assistant]".to_string(),
        (BlockKind::ToolCall, _) => format!("[tool call {}]", b.tool_name.as_deref().unwrap_or("unnamed")),
        (BlockKind::ToolResult, _) => "[tool result]".to_string(),
        (BlockKind::Drift, _) => "[drift]".to_string(),
        _ => return None,
    };
    let body = match (b.kind, b.content.is_empty()) {
        (BlockKind::ToolCall, true) => b.tool_input.as_deref().unwrap_or(""),
        _ => b.content.as_str(),
    };
    if body.is_empty() {
        return None;
    }
    Some(match body.len() > BLOCK_BYTES {
        true => format!("{label} {}<truncated {} bytes />", head(body, BLOCK_BYTES), body.len() - BLOCK_BYTES),
        false => format!("{label} {body}"),
    })
}

/// The longest prefix of `text` within `bytes`, on a char boundary.
fn head(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, BlockSnapshotBuilder, PrincipalId};

    const DENY_HIGH: &str =
        r#"{"outcome":"deny","risk_level":"high","user_authorization":"low","rationale":"It force-pushes main."}"#;

    #[test]
    fn a_full_answer_parses() {
        let a = parse_assessment(&format!("  {DENY_HIGH}\n")).unwrap();
        assert_eq!(
            a,
            Assessment {
                outcome: Verdict::Deny,
                risk_level: Risk::High,
                user_authorization: Authorization::Low,
                rationale: "It force-pushes main.".into(),
            }
        );
    }

    /// Strict: a short allow, prose, a code fence, an unknown field, an
    /// unknown word, and an empty rationale are each a miss cause.
    #[test]
    fn anything_but_the_full_object_is_refused_with_a_quote() {
        for bad in [
            r#"{"outcome":"allow"}"#.to_string(),
            format!("Here is my answer: {DENY_HIGH}"),
            format!("```json\n{DENY_HIGH}\n```"),
            DENY_HIGH.replace("}", r#","confidence":0.9}"#),
            DENY_HIGH.replace("\"high\"", "\"severe\""),
            DENY_HIGH.replace("It force-pushes main.", " "),
            String::new(),
        ] {
            let error = parse_assessment(&bad).unwrap_err();
            assert!(error.starts_with("the guardian's answer"), "{error}");
        }
        let long = "x".repeat(500);
        let error = parse_assessment(&long).unwrap_err();
        assert!(error.ends_with("...") && error.len() < 400, "{error}");
    }

    fn assessed(outcome: Verdict, risk: Risk) -> Assessment {
        Assessment { outcome, risk_level: risk, user_authorization: Authorization::Unknown, rationale: "r".into() }
    }

    #[test]
    fn verdicts_map_to_gate_outcomes() {
        use Risk::*;
        for risk in [Low, Medium, High] {
            assert_eq!(assessed(Verdict::Allow, risk).outcome(), Outcome::Allow, "{risk:?}");
            assert_eq!(assessed(Verdict::Deny, risk).outcome(), Outcome::Ask, "{risk:?}");
        }
        assert_eq!(assessed(Verdict::Deny, Critical).outcome(), Outcome::Report);
        assert_eq!(assessed(Verdict::Allow, Critical).outcome(), Outcome::Ask, "a contradictory allow asks");
    }

    #[test]
    fn three_denials_in_a_row_trip_the_breaker_once() {
        let mut b = Breaker::default();
        assert_eq!(b.record(Verdict::Deny), None);
        assert_eq!(b.record(Verdict::Deny), None);
        let cause = b.record(Verdict::Deny).expect("the third denial trips");
        assert!(cause.contains("3 submissions in a row"), "{cause}");
        assert_eq!(b.tripped(), Some(cause.as_str()));
        assert_eq!(b.record(Verdict::Deny), None, "it trips once");
        assert_eq!(b.record(Verdict::Allow), None);
        assert!(b.tripped().is_some(), "an allow does not reset a tripped breaker");
    }

    #[test]
    fn an_allow_resets_the_run_but_ten_of_fifty_still_trip() {
        let mut b = Breaker::default();
        for _ in 0..4 {
            assert_eq!(b.record(Verdict::Deny), None);
            assert_eq!(b.record(Verdict::Deny), None);
            assert_eq!(b.record(Verdict::Allow), None);
        }
        assert_eq!(b.record(Verdict::Deny), None, "nine of the last twelve");
        let cause = b.record(Verdict::Deny).expect("ten of the last thirteen");
        assert!(cause.contains("10 of the last 14"), "{cause}");
    }

    #[test]
    fn denials_older_than_the_window_do_not_count() {
        let mut b = Breaker::default();
        for _ in 0..9 {
            b.record(Verdict::Deny);
            b.record(Verdict::Allow);
        }
        for _ in 0..DENIAL_WINDOW {
            assert_eq!(b.record(Verdict::Allow), None);
        }
        for _ in 0..2 {
            assert_eq!(b.record(Verdict::Deny), None);
            assert_eq!(b.record(Verdict::Allow), None);
        }
        assert!(b.tripped().is_none());
    }

    #[test]
    fn breakers_are_per_context() {
        let breakers = Breakers::default();
        let (a, other) = (ContextId::new(), ContextId::new());
        for _ in 0..3 {
            breakers.record(a, Verdict::Deny);
        }
        assert!(breakers.tripped(a).is_some());
        assert!(breakers.tripped(other).is_none());
    }

    fn block(seq: u64, kind: BlockKind, role: Role, content: &str) -> BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(ContextId::new(), PrincipalId::new(), seq), kind)
            .role(role)
            .status(Status::Done)
            .content(content)
            .build()
    }

    #[test]
    fn the_transcript_labels_turns_and_tools_and_keeps_out_the_rest() {
        let blocks = vec![
            block(0, BlockKind::Text, Role::System, "system instructions"),
            block(1, BlockKind::Text, Role::User, "please fix the build"),
            block(2, BlockKind::Thinking, Role::Model, "private thought"),
            BlockSnapshotBuilder::new(BlockId::new(ContextId::new(), PrincipalId::new(), 3), BlockKind::ToolCall)
                .role(Role::Model)
                .status(Status::Done)
                .tool_name("shell")
                .tool_input(r#"{"command":"cargo build"}"#)
                .build(),
            block(4, BlockKind::ToolResult, Role::Tool, "error[E0425]"),
            block(5, BlockKind::Text, Role::Model, "I will fix it"),
            BlockSnapshotBuilder::new(BlockId::new(ContextId::new(), PrincipalId::new(), 6), BlockKind::Text)
                .role(Role::User)
                .status(Status::Done)
                .content("excluded")
                .excluded(true)
                .build(),
        ];
        assert_eq!(
            transcript(&blocks, 10_000),
            "[user] please fix the build\n[tool call shell] {\"command\":\"cargo build\"}\n[tool result] error[E0425]\n\
             [assistant] I will fix it"
        );
    }

    #[test]
    fn the_transcript_keeps_the_tail_within_its_budget_and_cuts_long_blocks() {
        let blocks: Vec<_> = (0..10).map(|i| block(i, BlockKind::Text, Role::User, &format!("turn {i}"))).collect();
        let text = transcript(&blocks, 42);
        assert!(text.starts_with("<truncated 7 earlier blocks />\n"), "{text}");
        assert!(text.ends_with("[user] turn 7\n[user] turn 8\n[user] turn 9"), "{text}");
        let long = block(0, BlockKind::ToolResult, Role::Tool, &"y".repeat(3000));
        let text = transcript(&[long], 10_000);
        assert!(text.ends_with("<truncated 952 bytes />") && text.len() < 2200, "{}", text.len());
    }
}
