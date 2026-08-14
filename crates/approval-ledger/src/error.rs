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

    /// Refused at the single write site that turns a decided approval's
    /// statement into a standing allow-always rule (guarantee 3): that
    /// statement has a `binding = 'free'` variable, so its
    /// `statement_digest` can never be made safe to auto-approve later —
    /// the statement is pre-resolution by design (`rm "$TARGET"`), and
    /// resolving it after the fact does not change what the digest
    /// identifies. Only raised for `allow` rules — a `deny` rule for the
    /// same free-variable statement is permitted (see
    /// `schema.rs`'s trigger doc for why: a standing deny is strictly
    /// safety-increasing, over-blocking it has no safety argument, and
    /// this crate does not create rules that never fire).
    #[error(
        "refusing to create an allow-always rule from request {request_id}: statement {stmt_seq} \
         has a free variable `${var_name}` — a rule on this statement_digest would authorize \
         whatever that variable resolves to on a future run, not what the human was shown"
    )]
    FreeVariableRule {
        request_id: String,
        stmt_seq: i64,
        var_name: String,
    },

    /// `learn_from_approval` was asked for a `stmt_seq` that has no entry
    /// in this ask's `approval_ask_statements` — either the position is
    /// out of range, or the ask has no statements at all (e.g. a
    /// `kj_verb`-origin ask with no shell plan). There is nothing to
    /// generalize; callers minting rules for verb-shaped asks need a real
    /// statement digest of their own to key on, which this path cannot
    /// invent.
    #[error("cannot learn a rule from request {request_id}: no statement at position {stmt_seq}")]
    NoSuchStatement { request_id: String, stmt_seq: i64 },

    /// The approval named in `learned_from` has no `authorized_label` —
    /// `approval_rules.authorized_label` is `NOT NULL` by design (see
    /// `schema.rs`), so there is nothing for a future redemption to
    /// compare against.
    #[error("cannot learn a rule from request {0}: it has no recorded authorized_label")]
    NoLabelRecorded(String),

    /// A redemption presented a label that does not match the rule's
    /// `authorized_label` for the same `statement_digest` — guarantee 4.
    /// This is intentionally a distinct variant from "no rule found":
    /// conflating "there IS a rule here but it doesn't cover what you're
    /// asking for" with "there is no rule at all" is exactly the
    /// silent-widening failure mode this guarantee exists to prevent, and
    /// it holds at the per-statement granularity `rules::redeem` now
    /// checks an ask's whole statement set at.
    #[error(
        "label mismatch redeeming a rule for statement_digest {statement_digest}: the rule authorizes \
         `{authorized_label}`, this redemption presented `{presented_label}` — refusing to widen scope"
    )]
    LabelMismatch {
        statement_digest: String,
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
