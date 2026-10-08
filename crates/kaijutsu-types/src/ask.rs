//! Typed views of the approval ledger for clients: an ask's card
//! ([`AskSummary`]), its full record ([`AskDetail`]), the listing filter, and
//! an answer. The kernel builds these from the ledger and `kj ledger
//! list|show` renders the same values, so the RPC and the shell agree.
//!
//! The ledger itself lives in `approval-ledger`, which this crate does not
//! depend on; the kernel maps between the two and a kernel test pins the
//! correspondence, as it does for [`AskStatus`].

use serde::{Deserialize, Serialize};

use crate::{AskStatus, BlockId, ContextId, PrincipalId};

/// Which path raised an ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskOrigin {
    /// A PreCall hook asks before execution.
    Hook,
    /// A result hook reviews captured output; approval never executes source.
    HookResult,
    /// The shell gate asked before executing a command.
    ShellGate,
    /// A privileged `kj` verb gated itself.
    KjVerb,
}

impl AskOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            AskOrigin::Hook => "hook",
            AskOrigin::HookResult => "hook_result",
            AskOrigin::ShellGate => "shell_gate",
            AskOrigin::KjVerb => "kj_verb",
        }
    }
}

impl std::fmt::Display for AskOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A principal and the name its character sheet gives it now. A principal
/// with no sheet carries its short id as the name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalRef {
    pub id: PrincipalId,
    pub name: String,
}

/// What a client needs to show an ask as a card or an indicator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskSummary {
    pub request_id: String,
    pub status: AskStatus,
    pub origin: AskOrigin,
    /// `None` when the stored context id is malformed.
    pub context_id: Option<ContextId>,
    pub description: String,
    /// Each statement the ask covers, rendered.
    pub statements: Vec<String>,
    /// Who submitted the request.
    pub requester: Option<PrincipalRef>,
    /// The character that performed the work which raised the ask.
    pub performer: Option<PrincipalRef>,
    /// The character assigned to answer it.
    pub reviewer: Option<PrincipalRef>,
    /// Unix-epoch milliseconds.
    pub created_at_ms: i64,
    /// Unix-epoch milliseconds; `None` while open.
    pub decided_at_ms: Option<i64>,
}

impl AskSummary {
    /// Whether `principal` may answer this ask now: it is open, and
    /// `principal` is its assigned reviewer. A row with no performer carries
    /// no review authority. The kernel remains authoritative if the
    /// assignment changes while a control is on screen.
    pub fn answerable_by(&self, principal: PrincipalId) -> bool {
        self.status.is_open() && self.performer.is_some()
            && self.reviewer.as_ref().is_some_and(|reviewer| reviewer.id == principal)
    }
}

/// One free variable's value at ask time. `value: None` means it was unset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskEnv {
    pub name: String,
    pub value: Option<String>,
}

/// How an ask was decided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskDecision {
    /// `None` when a rule decided it.
    pub decided_by: Option<PrincipalRef>,
    /// `allow_once`, `deny`, `prompt_cancelled`, `cancel`, `auto_allow`, and so on.
    /// Finer than the status.
    pub option: Option<String>,
    pub remember_scope: Option<String>,
    /// Why a rule or the kernel decided it without a person.
    pub auto_reason: Option<String>,
}

/// An ask's full record: what it covers, what an approval runs with, and
/// how it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskDetail {
    pub summary: AskSummary,
    pub instance: Option<String>,
    pub tool: Option<String>,
    pub hook_id: Option<String>,
    pub label: Option<String>,
    /// The model's ToolCall block whose invocation raised the ask.
    pub tool_call_block_id: Option<BlockId>,
    pub exec_source: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<AskEnv>,
    /// `None` while open, and on an ask that expired or was swept without
    /// anyone deciding it. A cancellation is a decision by whoever
    /// cancelled.
    pub decision: Option<AskDecision>,
    /// When the answer was consumed. Consumption can deliver a refusal or
    /// retire an unpublished invocation; it does not prove source ran.
    pub redeemed_at_ms: Option<i64>,
    /// Why the caller retired the invocation before publishing its result.
    pub publication_abandoned: Option<String>,
    /// Each time the ask changed reviewer, oldest first.
    pub reassignments: Vec<AskReassignment>,
}

/// One change of an ask's reviewer. `by == to` is a takeover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskReassignment {
    /// `None` on an ask that had no reviewer recorded.
    pub from: Option<PrincipalRef>,
    pub to: PrincipalRef,
    pub by: PrincipalRef,
    /// Unix-epoch milliseconds.
    pub at_ms: i64,
}

/// Which asks a listing covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskView {
    /// Pending asks, oldest first.
    #[default]
    Queue,
    /// Decided asks, newest first.
    History,
}

/// A ledger listing's filter. A decided `status` selects the history view by
/// itself, as `kj ledger list --status` does.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskFilter {
    pub view: AskView,
    pub status: Option<AskStatus>,
    pub origin: Option<AskOrigin>,
    /// Only asks created at or after this unix-epoch millisecond.
    pub since_ms: Option<i64>,
    /// `None` lists every matching ask.
    pub limit: Option<u32>,
}

/// An answer to an ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskVerdict {
    Allow,
    Deny,
    /// The reviewer's prompt was cancelled rather than answered: a denial
    /// recorded as `prompt_cancelled`.
    PromptCancelled,
}

/// How far a remembered answer reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RememberScope {
    /// The context and principal that asked.
    Session,
    /// Any context or principal presenting the same statement and label.
    Always,
}

/// A standing rule an answer asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remember {
    pub scope: RememberScope,
    /// Remember the command family rather than the exact text.
    pub family: bool,
}

/// An answer or reassignment the ledger accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswered {
    /// The ask after the change.
    pub summary: AskSummary,
    /// What became of a requested standing rule. `None` when none was
    /// requested.
    pub remembered: Option<RememberResult>,
}

/// Whether a requested standing rule was written. The answer stands either
/// way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberResult {
    pub learned: bool,
    /// What was learned, or why it was not.
    pub note: String,
}

/// Why the ledger did not accept an answer or reassignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskAnswerFailureKind {
    NotFound,
    /// The caller is not the assigned reviewer, or performed the work.
    NotReviewer,
    /// Another answer, an expiry, or a cancellation got there first.
    AlreadyAnswered,
    /// The ask's context is archived and runs nothing.
    Archived,
    /// The request was invalid or the ledger refused it for another reason.
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswerFailure {
    pub kind: AskAnswerFailureKind,
    pub message: String,
}

impl std::fmt::Display for AskAnswerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AskAnswerFailure {}
