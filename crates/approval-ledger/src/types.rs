//! Enumerations and row/input types for the approval ledger.
//!
//! Enum shape follows `kaijutsu-types/src/enums.rs`: `strum::EnumString`
//! for parsing (`FromStr`), a hand-written `as_str`/`Display` for the
//! reverse direction, so the two can never quietly disagree. Every enum
//! here also has a `CHECK` constraint on its column in `schema.rs` —
//! belt (storage refuses the bad value) and suspenders (a value that
//! somehow got in some other way still fails a clean, typed parse instead
//! of being silently treated as a default).
//!
//! `context_id` / `principal_id` / `claimed_by` / `decided_by` / `actor` /
//! `created_by` are plain `Vec<u8>` (BLOB), not a typed id, because this
//! crate has zero dependency on `kaijutsu-types` by design (see crate
//! docs) — the kernel's `ContextId`/`PrincipalId` are UUID-backed 16-byte
//! blobs; callers convert at the boundary.
//!
//! Every enum and every `*Row` read-back type derives `Serialize`/
//! `Deserialize` — a caller (kernel, MCP, a future ACP surface) can hand
//! one of these across a wire without a bespoke DTO. The `New*` input
//! builder types deliberately do not: they're constructed in Rust by a
//! caller that already has a parsed `kaish_types::plan::Plan` in hand (or
//! its own equivalent), not deserialized directly off the wire.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use strum::EnumString;

// ============================================================================
// Enums
// ============================================================================

/// Where an approval request originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A hook's `ask` action fired.
    Hook,
    /// The shell tool gated a command before executing it.
    ShellGate,
    /// A privileged `kj` verb (e.g. `context archive`) gated itself.
    KjVerb,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::ShellGate => "shell_gate",
            Self::KjVerb => "kj_verb",
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state of an approval request. `Pending` and `Claimed` are the
/// two live states; the other four are terminal (guarantee 6: once here,
/// never leaves — enforced by `approvals_decided_is_immutable`, and see
/// guarantee 2: `Expired`/`Abandoned` are both NOT-allowed, same as
/// `Denied`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Claimed,
    Allowed,
    Denied,
    Expired,
    Abandoned,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Abandoned => "abandoned",
        }
    }

    /// True only for `Allowed`. This is the single decision point guarantee
    /// 2 depends on: every other variant, known or not-yet-invented,
    /// resolves to `false` by simply not matching this arm — there is no
    /// `_ => true` anywhere in this crate.
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// True for the four states `approvals_decided_is_immutable` refuses to
    /// leave.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Allowed | Self::Denied | Self::Expired | Self::Abandoned)
    }
}

impl fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a plan value (an argument or a redirect target) was judged
/// secret. Mirrors kaish-types' `PlannedValue` exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    Plain,
    Redacted,
}

impl ValueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Redacted => "redacted",
        }
    }
}

impl fmt::Display for ValueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a statement reads a variable without lexically binding it
/// (`free`) or binds it itself (`bound`) — mirrors kaish's
/// `Plan::free_variables` / `bound_variables`. `Free` is the one value
/// guarantee 3 cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum VarBinding {
    Free,
    Bound,
}

impl VarBinding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Bound => "bound",
        }
    }
}

impl fmt::Display for VarBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an advisory signal came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SignalSourceKind {
    Rule,
    Classifier,
}

impl SignalSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Classifier => "classifier",
        }
    }
}

impl fmt::Display for SignalSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An advisory signal's recommendation. Never itself a gate — see
/// `approval_signals` in `schema.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum SignalVerdict {
    Escalate,
    Deny,
    Allow,
}

impl SignalVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Escalate => "escalate",
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

impl fmt::Display for SignalVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How broadly a standing rule applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    /// Only within the `context_id`/`principal_id` the rule recorded.
    Session,
    /// Any context/principal presenting the same `statement_digest` + label.
    Always,
}

impl RuleScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Always => "always",
        }
    }
}

impl fmt::Display for RuleScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One append-only `approval_events` row's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Claimed,
    Decided,
    Expired,
    Abandoned,
    /// A decide() attempt that arrived after the request already reached a
    /// terminal status — guarantee 6's "recorded, never silently dropped
    /// or applied".
    LateDecision,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Decided => "decided",
            Self::Expired => "expired",
            Self::Abandoned => "abandoned",
            Self::LateDecision => "late_decision",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of one rc lifecycle run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RcOutcome {
    Ok,
    Failed,
    Abandoned,
}

impl RcOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for RcOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse a `CHECK`-constrained TEXT column into its enum, mapping a
/// parse failure into a self-describing panic-free error string. Every
/// row-reconstruction function in this crate routes an unrecognized value
/// through this instead of `unwrap_or(default)` — see guarantee 2: an
/// unrecognized status must never be silently treated as anything, let
/// alone allowed.
pub(crate) fn parse_enum<T: FromStr>(column: &'static str, raw: &str) -> Result<T, crate::error::LedgerError> {
    T::from_str(raw).map_err(|_| {
        crate::error::LedgerError::Db(rusqlite::Error::InvalidColumnType(
            0,
            format!("{column}: unrecognized value {raw:?}"),
            rusqlite::types::Type::Text,
        ))
    })
}

// ============================================================================
// Plan tree — input shapes (mirror kaish's Plan / PlannedCommand /
// PlannedValue / PlannedRedirect one level at a time)
// ============================================================================

/// One value inside a plan (an argument or a redirect target) — mirrors
/// kaish-types' `PlannedValue`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewPlannedValue {
    Plain(String),
    Redacted {
        redact_kind: String,
        fingerprint: Option<String>,
    },
}

impl NewPlannedValue {
    pub(crate) fn kind(&self) -> ValueKind {
        match self {
            Self::Plain(_) => ValueKind::Plain,
            Self::Redacted { .. } => ValueKind::Redacted,
        }
    }
}

/// One command's redirect, ready to insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanRedirect {
    /// The operator as rendered: `">"`, `">>"`, `"2>"`, `"<"`, `"2>&1"`, …
    pub op: String,
    pub target: NewPlannedValue,
}

/// One command inside a statement, ready to insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanCommand {
    pub name: String,
    pub args: Vec<NewPlannedValue>,
    pub redirects: Vec<NewPlanRedirect>,
    pub backgrounded: bool,
}

/// One variable a statement reads or binds, ready to insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanVar {
    pub name: String,
    pub binding: VarBinding,
}

/// One top-level statement, ready to insert, carrying its own
/// content-addressed identity. Since Amy's 2026-08-14 ruling (see
/// `schema.rs` header), the digest lives on the statement itself, not on
/// a wrapping "plan" — `NewAsk::statements` is simply an ordered `Vec` of
/// these, the same shape `options`/`signals` already use (position in the
/// `Vec` becomes `stmt_seq`). `create_ask` inserts a statement's tree only
/// the first time its `statement_digest` is seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanStatement {
    pub statement_digest: String,
    pub rendered: String,
    pub statement_kind: String,
    pub commands: Vec<NewPlanCommand>,
    pub vars: Vec<NewPlanVar>,
}

// ============================================================================
// New* input types for the top-level write functions
// ============================================================================

/// One offered choice, ready to insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewOption {
    pub option_id: String,
    pub label: String,
    pub kind: String,
}

/// One advisory signal, ready to insert. `PartialEq` only (not `Eq`) —
/// `score` is an `f64`.
#[derive(Clone, Debug, PartialEq)]
pub struct NewSignal {
    pub source_kind: SignalSourceKind,
    pub source_id: Option<String>,
    pub model_id: Option<String>,
    pub weight_hash: Option<String>,
    pub stmt_seq: Option<i64>,
    pub cmd_seq: Option<i64>,
    pub label: Option<String>,
    pub score: Option<f64>,
    pub verdict: SignalVerdict,
}

/// Everything `ask::create_ask` needs to durably record one ask before any
/// human is prompted (guarantee 1). `statements` is the ask's ORDERED
/// statement list — empty for an ask with no shell plan at all (e.g. a
/// `kj_verb`-origin ask), position in the `Vec` becomes `stmt_seq` in
/// `approval_ask_statements`, same as `options`/`signals`.
#[derive(Clone, Debug)]
pub struct NewAsk {
    pub context_id: Vec<u8>,
    pub principal_id: Vec<u8>,
    pub origin: Origin,
    pub instance: Option<String>,
    pub tool: Option<String>,
    pub hook_id: Option<String>,
    pub description: String,
    pub statements: Vec<NewPlanStatement>,
    pub authorized_label: Option<String>,
    pub rc_run_id: Option<String>,
    pub expires_at: Option<i64>,
    pub options: Vec<NewOption>,
    pub signals: Vec<NewSignal>,
}

// ============================================================================
// Row types — what a read gives back
// ============================================================================

/// One `approvals` row. No `plan_digest` field — an ask's statement set
/// is many-to-many via `approval_ask_statements`
/// (`ask::load_ask_statements`); see `schema.rs` header for why this
/// crate refuses to cache a derived ask-level digest here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRow {
    pub request_id: String,
    pub context_id: Vec<u8>,
    pub principal_id: Vec<u8>,
    pub origin: Origin,
    pub instance: Option<String>,
    pub tool: Option<String>,
    pub hook_id: Option<String>,
    pub description: String,
    pub authorized_label: Option<String>,
    pub rc_run_id: Option<String>,
    pub status: ApprovalStatus,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub claimed_at: Option<i64>,
    pub claimed_by: Option<Vec<u8>>,
    pub decided_at: Option<i64>,
    pub decided_by: Option<Vec<u8>>,
    pub decided_option: Option<String>,
    pub remember_scope: Option<String>,
    pub auto_reason: Option<String>,
}

/// One `approval_options` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionRow {
    pub seq: i64,
    pub option_id: String,
    pub label: String,
    pub kind: String,
}

/// One `approval_signals` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignalRow {
    pub seq: i64,
    pub source_kind: SignalSourceKind,
    pub source_id: Option<String>,
    pub model_id: Option<String>,
    pub weight_hash: Option<String>,
    pub stmt_seq: Option<i64>,
    pub cmd_seq: Option<i64>,
    pub label: Option<String>,
    pub score: Option<f64>,
    pub verdict: SignalVerdict,
}

/// One `approval_events` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRow {
    pub seq: i64,
    pub kind: EventKind,
    pub actor: Option<Vec<u8>>,
    pub decided_option: Option<String>,
    pub remember_scope: Option<String>,
    pub auto_reason: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
}

/// A reconstructed plan value (argument or redirect target).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedValueRow {
    Plain(String),
    Redacted {
        redact_kind: String,
        fingerprint: Option<String>,
    },
}

/// One reconstructed `approval_plan_redirects` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRedirectRow {
    pub redir_seq: i64,
    pub op: String,
    pub target: PlannedValueRow,
}

/// One reconstructed `approval_plan_commands` row, joined with its args
/// and redirects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCommandRow {
    pub cmd_seq: i64,
    pub name: String,
    pub args: Vec<PlannedValueRow>,
    pub redirects: Vec<PlanRedirectRow>,
    pub backgrounded: bool,
}

/// One reconstructed `approval_statements` row, joined with its commands
/// and variables. What `ask::load_statement` returns — a statement's
/// content is self-contained now (identified by `statement_digest`, not
/// by any one ask's use of it), so this carries the digest instead of a
/// `stmt_seq`; see [`AskStatementRow`] for the ask-relative wrapper
/// `ask::load_ask_statements` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStatementRow {
    pub statement_digest: String,
    pub rendered: String,
    pub statement_kind: String,
    pub commands: Vec<PlanCommandRow>,
    pub free_vars: Vec<String>,
    pub bound_vars: Vec<String>,
}

/// One statement in an ask's ORDERED list (`approval_ask_statements`),
/// joined against its content-addressed body. What
/// `ask::load_ask_statements` returns, one per statement, in `stmt_seq`
/// order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskStatementRow {
    pub stmt_seq: i64,
    pub statement: PlanStatementRow,
}

/// One `approval_rules` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleRow {
    pub rule_id: String,
    pub statement_digest: String,
    pub authorized_label: String,
    pub context_id: Option<Vec<u8>>,
    pub principal_id: Option<Vec<u8>>,
    pub scope: RuleScope,
    pub allow: bool,
    pub created_at: i64,
    pub created_by: Option<Vec<u8>>,
    pub learned_from: Option<String>,
    pub revoked_at: Option<i64>,
}

/// One statement's coverage result within `rules::redeem` — guarantee 4
/// preserved at per-statement granularity: `Uncovered` (no rule at all —
/// clean cache miss) is never conflated with a rule that exists but
/// disagrees on label (that path is a loud [`crate::error::LedgerError::LabelMismatch`],
/// not a `StatementVerdict` variant, so it can never be silently folded
/// into "uncovered").
#[derive(Clone, Debug)]
pub enum StatementVerdict {
    /// Covered by an active `allow` rule matching the presented label.
    Allow(RuleRow),
    /// Covered by an active `deny` rule matching the presented label.
    Deny(RuleRow),
    /// No active rule at all covers this statement in this scope.
    Uncovered,
}

/// The composed result of checking a whole ask's statement set against
/// active rules (`rules::redeem`). Generalization is per-statement (Amy,
/// 2026-08-14 — see `schema.rs` header), so an ask's own verdict is a
/// composition, not a single lookup: `verdict()` denies if ANY statement
/// is covered by a deny rule (deny short-circuits, strictly
/// safety-increasing), allows only if EVERY statement is covered by an
/// allow rule, and escalates otherwise — a partially-covered ask is never
/// partially applied.
#[derive(Clone, Debug)]
pub struct AskCoverage {
    /// One entry per statement digest passed to `redeem`, same order.
    pub per_statement: Vec<StatementVerdict>,
}

/// The composed auto-decision outcome for a whole ask. See
/// [`AskCoverage::verdict`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskVerdict {
    /// At least one statement is covered by an active `deny` rule.
    Deny,
    /// Every statement (at least one) is covered by an active `allow`
    /// rule and none are denied.
    Allow,
    /// Not fully covered and not denied — including an empty statement
    /// set, which is never vacuously `Allow` (nothing to check is not the
    /// same as everything checked out). Ask a human.
    Escalate,
}

impl AskCoverage {
    pub fn verdict(&self) -> AskVerdict {
        if self.per_statement.iter().any(|v| matches!(v, StatementVerdict::Deny(_))) {
            return AskVerdict::Deny;
        }
        if !self.per_statement.is_empty()
            && self.per_statement.iter().all(|v| matches!(v, StatementVerdict::Allow(_)))
        {
            return AskVerdict::Allow;
        }
        AskVerdict::Escalate
    }
}

/// One `rc_runs` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RcRunRow {
    pub run_id: String,
    pub context_id: Vec<u8>,
    pub context_type: String,
    pub verb: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub outcome: Option<RcOutcome>,
}

/// One `rc_run_scripts` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RcRunScriptRow {
    pub seq: i64,
    pub path: String,
    pub body_sha256: String,
    pub exit_code: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_enum_as_str_round_trips_through_from_str() {
        for v in [Origin::Hook, Origin::ShellGate, Origin::KjVerb] {
            assert_eq!(Origin::from_str(v.as_str()).unwrap(), v);
        }
        for v in [
            ApprovalStatus::Pending,
            ApprovalStatus::Claimed,
            ApprovalStatus::Allowed,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
            ApprovalStatus::Abandoned,
        ] {
            assert_eq!(ApprovalStatus::from_str(v.as_str()).unwrap(), v);
        }
        for v in [ValueKind::Plain, ValueKind::Redacted] {
            assert_eq!(ValueKind::from_str(v.as_str()).unwrap(), v);
        }
        for v in [VarBinding::Free, VarBinding::Bound] {
            assert_eq!(VarBinding::from_str(v.as_str()).unwrap(), v);
        }
        for v in [RuleScope::Session, RuleScope::Always] {
            assert_eq!(RuleScope::from_str(v.as_str()).unwrap(), v);
        }
        for v in [RcOutcome::Ok, RcOutcome::Failed, RcOutcome::Abandoned] {
            assert_eq!(RcOutcome::from_str(v.as_str()).unwrap(), v);
        }
    }

    #[test]
    fn an_unrecognized_status_fails_to_parse_rather_than_defaulting() {
        // Guarantee 2's read-side half: nothing in this crate maps an
        // unrecognized value to a default `ApprovalStatus` (which could
        // accidentally be `Allowed`) — it's a hard parse error instead.
        assert!(ApprovalStatus::from_str("sideways").is_err());
    }

    #[test]
    fn is_allowed_is_true_for_exactly_one_variant() {
        let all = [
            ApprovalStatus::Pending,
            ApprovalStatus::Claimed,
            ApprovalStatus::Allowed,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
            ApprovalStatus::Abandoned,
        ];
        let allowed_count = all.iter().filter(|s| s.is_allowed()).count();
        assert_eq!(allowed_count, 1);
        assert!(ApprovalStatus::Allowed.is_allowed());
    }
}
