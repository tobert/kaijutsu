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
    /// Any context/principal presenting the same `plan_digest` + label.
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

/// One top-level statement, ready to insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlanStatement {
    pub rendered: String,
    pub statement_kind: String,
    pub commands: Vec<NewPlanCommand>,
    pub vars: Vec<NewPlanVar>,
}

/// A full plan tree plus the digest that identifies it, ready to insert
/// (content-addressed — see `schema.rs` header). `create_ask` inserts the
/// tree only the first time a `plan_digest` is seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPlan {
    pub plan_digest: String,
    pub statements: Vec<NewPlanStatement>,
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
/// human is prompted (guarantee 1).
#[derive(Clone, Debug)]
pub struct NewAsk {
    pub context_id: Vec<u8>,
    pub principal_id: Vec<u8>,
    pub origin: Origin,
    pub instance: Option<String>,
    pub tool: Option<String>,
    pub hook_id: Option<String>,
    pub description: String,
    pub plan: Option<NewPlan>,
    pub authorized_label: Option<String>,
    pub rc_run_id: Option<String>,
    pub expires_at: Option<i64>,
    pub options: Vec<NewOption>,
    pub signals: Vec<NewSignal>,
}

// ============================================================================
// Row types — what a read gives back
// ============================================================================

/// One `approvals` row.
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
    pub plan_digest: Option<String>,
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

/// One reconstructed `approval_plan_statements` row, joined with its
/// commands and variables. What `load_plan` returns, per statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStatementRow {
    pub stmt_seq: i64,
    pub rendered: String,
    pub statement_kind: String,
    pub commands: Vec<PlanCommandRow>,
    pub free_vars: Vec<String>,
    pub bound_vars: Vec<String>,
}

/// One `approval_rules` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleRow {
    pub rule_id: String,
    pub plan_digest: String,
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
