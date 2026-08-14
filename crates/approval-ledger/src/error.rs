//! Error type for every fallible operation in this crate.
//!
//! Following `KernelDbError`'s shape (`kaijutsu-kernel/src/kernel_db.rs`):
//! one `thiserror` enum, `#[from] rusqlite::Error` for the underlying
//! database, and a distinct variant per invariant this crate exists to
//! protect — so a caller can `match` on *which* guarantee tripped instead
//! of string-matching an error message.

use thiserror::Error;

/// Errors from approval-ledger operations.
#[derive(Debug, Error)]
pub enum LedgerError {
    /// Underlying SQLite error — includes the `RAISE(ABORT, ...)` from this
    /// crate's own triggers (`schema::DDL`) when a write path bypasses the
    /// Rust-level check that normally catches the same violation first.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// No approval with this `request_id` exists.
    #[error("approval request {0} not found")]
    NotFound(String),

    /// The request already reached a terminal status (`allowed`, `denied`,
    /// `expired`, `abandoned`) — guarantee 6. Carries the terminal status
    /// so a caller can tell "this was already decided" from "this timed
    /// out before you answered" without a second query. A late answer
    /// hitting this path is still recorded — see
    /// [`crate::decide::decide`]'s doc comment.
    #[error("approval request {request_id} already reached terminal status `{status}`; re-deciding is refused")]
    AlreadyDecided { request_id: String, status: String },

    /// The request is not in `pending` — either already claimed by someone
    /// else (guarantee 5's losing side) or already terminal.
    #[error("approval request {request_id} cannot be claimed: status is `{status}`, not `pending`")]
    NotClaimable { request_id: String, status: String },

    /// Refused at the single write site that turns a decided approval into
    /// a standing allow-always rule (guarantee 3): the approval's plan has
    /// at least one `binding = 'free'` variable in some statement, so its
    /// `plan_digest` can never be made safe to auto-approve later — the
    /// plan is pre-resolution by design (`rm "$TARGET"`), and resolving it
    /// after the fact does not change what the digest identifies.
    #[error(
        "refusing to create an allow-always rule from request {request_id}: statement {stmt_seq} \
         has a free variable `${var_name}` — a rule on this plan_digest would authorize whatever \
         that variable resolves to on a future run, not what the human was shown"
    )]
    FreeVariableRule {
        request_id: String,
        stmt_seq: i64,
        var_name: String,
    },

    /// The approval named in `learned_from` has no recorded plan
    /// (`plan_digest` is NULL) — there is nothing to check for free
    /// variables, so refuse rather than guess. A `kj_verb`-origin approval
    /// with no shell plan should never be generalized into a
    /// `plan_digest`-keyed rule through this path; callers minting rules
    /// for verb-shaped asks need a real digest of their own to key on.
    #[error("cannot learn a rule from request {0}: it has no recorded plan_digest")]
    NoPlanRecorded(String),

    /// The approval named in `learned_from` has no `authorized_label` —
    /// `approval_rules.authorized_label` is `NOT NULL` by design (see
    /// `schema.rs`), so there is nothing for a future redemption to
    /// compare against.
    #[error("cannot learn a rule from request {0}: it has no recorded authorized_label")]
    NoLabelRecorded(String),

    /// A redemption presented a label that does not match the rule's
    /// `authorized_label` for the same `plan_digest` — guarantee 4. This is
    /// intentionally a distinct variant from "no rule found": conflating
    /// "there IS a rule here but it doesn't cover what you're asking for"
    /// with "there is no rule at all" is exactly the silent-widening
    /// failure mode this guarantee exists to prevent.
    #[error(
        "label mismatch redeeming a rule for plan_digest {plan_digest}: the rule authorizes \
         `{authorized_label}`, this redemption presented `{presented_label}` — refusing to widen scope"
    )]
    LabelMismatch {
        plan_digest: String,
        authorized_label: String,
        presented_label: String,
    },

    /// No approval_rules row exists with this `rule_id`.
    #[error("rule {0} not found")]
    RuleNotFound(String),

    /// No `rc_runs` row exists with this `run_id`.
    #[error("rc run {0} not found")]
    RunNotFound(String),

    /// `finish_run` called on a run that already has a `finished_at` —
    /// same immutability spirit as guarantee 6, applied to the run log:
    /// a finished run's outcome does not silently get overwritten either.
    #[error("rc run {0} was already finished")]
    RunAlreadyFinished(String),
}

pub type Result<T> = std::result::Result<T, LedgerError>;
