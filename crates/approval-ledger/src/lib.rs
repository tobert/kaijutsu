//! A durable, SQLite-backed ledger for human-in-the-loop authorization of
//! agent actions.
//!
//! An "ask" becomes a row **before** anyone is prompted; a human answers
//! it later — possibly minutes later, possibly from a different UI (CLI,
//! ACP client, GUI). This is explicitly **not** a blocking callback.
//!
//! ## Shape
//!
//! - **Standalone.** No dependency on any `kaijutsu-*` crate — publishable
//!   on its own someday.
//! - **The database is injected.** Every function takes a borrowed
//!   `&Connection`; this crate never opens or owns one (precedent:
//!   `refinery`'s `runner().run(&mut conn)`). Call [`schema::migrate`]
//!   once per process against whatever connection the caller manages.
//!   Tests use `Connection::open_in_memory()` — except the claim-race
//!   test, which needs a real on-disk file (two in-memory connections
//!   don't share a database).
//! - **Storage + invariants only.** No kaish integration, no HTTP/
//!   classifier client, no `kj` verbs, no wire/capnp types, no kernel
//!   wiring. This crate answers "is this durably recorded, and does it
//!   obey its own rules" — never "should this be allowed", which is
//!   policy that belongs above it.
//!
//! ## The six guarantees this crate exists to hold
//!
//! 1. **Durable before asked** — [`ask::create_ask`] commits before
//!    returning a `request_id`; nothing shows a human a prompt for a row
//!    that isn't already on disk.
//! 2. **Fail-closed on every terminal path** —
//!    [`types::ApprovalStatus::is_allowed`] is the one predicate for "was
//!    this actually allowed", true for exactly one variant; timeout
//!    ([`decide::expire`]), abandonment ([`decide::abandon`]), and any
//!    unparseable status all read `false` through it.
//! 3. **Free variables are never eligible for allow-always** —
//!    [`rules::learn_from_approval`] is the one write site that creates a
//!    rule, and it refuses when the approval's plan has any
//!    `binding = 'free'` variable; `schema.rs`'s
//!    `approval_rules_reject_free_variable_plans` trigger backstops it.
//! 4. **Label-not-id scoping** — [`rules::redeem`] never conflates "no
//!    rule for this digest" with "a rule exists but for a different
//!    label"; the second case is a distinct, loud
//!    [`error::LedgerError::LabelMismatch`].
//! 5. **Exactly one answerer wins** — [`claim::claim`] /
//!    [`claim::claim_next`] use `BEGIN IMMEDIATE` plus one atomic
//!    `UPDATE ... WHERE status = 'pending' ... RETURNING`.
//! 6. **A decided ask is immutable** — [`decide::decide`] can never
//!    overwrite a terminal row (the `UPDATE` matches zero rows for one);
//!    a late answer is still recorded, as a `late_decision` row in
//!    `approval_events`, before the call returns
//!    [`error::LedgerError::AlreadyDecided`].
//!
//! See `schema.rs`'s module doc for the relational design behind these —
//! it was reasoned backwards from the guarantees (including a second
//! opinion from an outside model), not forward-ported from a wire shape.

pub mod ask;
pub mod claim;
pub mod decide;
pub mod error;
mod events;
pub mod rc_runs;
pub mod rules;
pub mod schema;
mod time;
pub mod types;

pub use error::{LedgerError, Result};
pub use schema::migrate;

/// Shared test fixtures — a fresh in-memory migrated `Connection`, and the
/// minimal `NewAsk`/`NewPlan` builders every module's unit tests start
/// from, so each guarantee's test is about the guarantee, not about
/// re-deriving a valid ask from scratch. Integration tests under `tests/`
/// (which only see the public API, and need an on-disk file rather than
/// in-memory for the concurrency/durability guarantees) keep their own
/// small local copies instead of depending on this `#[cfg(test)]`-only
/// module.
#[cfg(test)]
pub(crate) mod fixtures {
    use rusqlite::Connection;

    use crate::types::{
        NewAsk, NewOption, NewPlan, NewPlanCommand, NewPlanStatement, NewPlanVar, NewPlannedValue,
        Origin, VarBinding,
    };

    pub(crate) fn open_memory() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        crate::schema::migrate(&conn).expect("migrate");
        conn
    }

    pub(crate) fn minimal_ask() -> NewAsk {
        NewAsk {
            context_id: vec![1, 2, 3, 4],
            principal_id: vec![9, 9, 9],
            origin: Origin::ShellGate,
            instance: Some("builtin.shell".into()),
            tool: Some("shell".into()),
            hook_id: None,
            description: "rm -rf ${TARGET}".into(),
            plan: None,
            authorized_label: Some("rm target".into()),
            rc_run_id: None,
            expires_at: None,
            options: vec![
                NewOption { option_id: "allow_once".into(), label: "Allow once".into(), kind: "allow_once".into() },
                NewOption { option_id: "deny".into(), label: "Deny".into(), kind: "deny".into() },
            ],
            signals: vec![],
        }
    }

    /// One statement, one command (`rm ${TARGET}`), one variable tagged
    /// `binding` — the fixture guarantee 3's tests are built on: a `Free`
    /// binding must block rule creation, a `Bound` one must not.
    pub(crate) fn plan_with_var(digest: &str, binding: VarBinding) -> NewPlan {
        NewPlan {
            plan_digest: digest.to_string(),
            statements: vec![NewPlanStatement {
                rendered: "rm ${TARGET}".into(),
                statement_kind: "command".into(),
                commands: vec![NewPlanCommand {
                    name: "rm".into(),
                    args: vec![NewPlannedValue::Plain("${TARGET}".into())],
                    redirects: vec![],
                    backgrounded: false,
                }],
                vars: vec![NewPlanVar { name: "TARGET".into(), binding }],
            }],
        }
    }

    pub(crate) fn ask_with_plan(digest: &str, binding: VarBinding, label: &str) -> NewAsk {
        let mut ask = minimal_ask();
        ask.plan = Some(plan_with_var(digest, binding));
        ask.authorized_label = Some(label.to_string());
        ask
    }
}
