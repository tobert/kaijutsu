//! Standing "remember this" rules — generalizing a decided approval into a
//! rule that can auto-decide future asks (guarantees 3 and 4).
//!
//! [`learn_from_approval`] is guarantee 3's single write site: it is the
//! only function in this crate that inserts into `approval_rules`, and it
//! refuses — with [`LedgerError::FreeVariableRule`] naming the offending
//! statement and variable — when the approval's plan carries any
//! `binding = 'free'` variable. `schema.rs`'s
//! `approval_rules_reject_free_variable_plans` trigger is the backstop for
//! any future write path that bypasses this function.
//!
//! [`redeem`] is guarantee 4: it never conflates "no rule covers this
//! digest" with "a rule covers this digest but not what you presented".

use rusqlite::{Connection, OptionalExtension, params};

use crate::ask::get_approval;
use crate::error::{LedgerError, Result};
use crate::time::now_millis;
use crate::types::{RuleRow, RuleScope, parse_enum};

/// Generalize a decided approval into a standing rule. `rule_id` is
/// generated (UUIDv7) if not given. Fails with
/// [`LedgerError::NotFound`] if `request_id` doesn't exist,
/// [`LedgerError::NoPlanRecorded`] if it has no `plan_digest`,
/// [`LedgerError::NoLabelRecorded`] if it has no `authorized_label`, and
/// — the guarantee this function exists to protect —
/// [`LedgerError::FreeVariableRule`] if any statement in its plan reads an
/// unresolved variable. None of these leave a partial row: the free-var
/// check runs before any `INSERT`.
pub fn learn_from_approval(
    conn: &Connection,
    request_id: &str,
    scope: RuleScope,
    allow: bool,
    created_by: Option<&[u8]>,
) -> Result<RuleRow> {
    let approval = get_approval(conn, request_id)?.ok_or_else(|| LedgerError::NotFound(request_id.to_string()))?;
    let plan_digest = approval
        .plan_digest
        .ok_or_else(|| LedgerError::NoPlanRecorded(request_id.to_string()))?;
    let authorized_label = approval
        .authorized_label
        .ok_or_else(|| LedgerError::NoLabelRecorded(request_id.to_string()))?;

    // The authoritative check (guarantee 3): query the plan's own variable
    // rows directly rather than trusting `approval_plans.has_free_vars` —
    // that cache exists to make the SCHEMA trigger cheap, not to be this
    // function's source of truth. Deterministic pick (lowest stmt_seq,
    // then name) so the error always names the same offender for the same
    // plan.
    let offender: Option<(i64, String)> = conn
        .query_row(
            "SELECT stmt_seq, name FROM approval_plan_vars
             WHERE plan_digest = ?1 AND binding = 'free'
             ORDER BY stmt_seq, name LIMIT 1",
            params![plan_digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((stmt_seq, var_name)) = offender {
        return Err(LedgerError::FreeVariableRule {
            request_id: request_id.to_string(),
            stmt_seq,
            var_name,
        });
    }

    let rule_id = uuid::Uuid::now_v7().to_string();
    let now = now_millis();
    conn.execute(
        "INSERT INTO approval_rules (
            rule_id, plan_digest, authorized_label, context_id, principal_id,
            scope, allow, created_at, created_by, learned_from
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            rule_id,
            plan_digest,
            authorized_label,
            approval.context_id,
            approval.principal_id,
            scope.as_str(),
            allow as i64,
            now,
            created_by,
            request_id,
        ],
    )?;

    get_rule(conn, &rule_id)?.ok_or_else(|| LedgerError::RuleNotFound(rule_id.clone()))
}

/// Look up a live rule for `plan_digest` that covers `presented_label`
/// within the given scope (an `always`-scope rule always qualifies; a
/// `session`-scope rule only if `context_id`/`principal_id` match it
/// exactly).
///
/// Three distinguishable outcomes (guarantee 4):
/// - `Ok(None)` — no rule at all covers this digest+scope. A clean
///   cache-miss; ask a human.
/// - `Ok(Some(rule))` — a rule covers it AND its `authorized_label`
///   matches `presented_label`. Safe to auto-decide.
/// - `Err(LabelMismatch)` — a rule covers this digest+scope but authorizes
///   a *different* label than what's being presented now. This is never
///   folded into `Ok(None)`: a scope violation must be loud, not
///   indistinguishable from an ordinary cache-miss.
pub fn redeem(
    conn: &Connection,
    plan_digest: &str,
    presented_label: &str,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
) -> Result<Option<RuleRow>> {
    let mut stmt = conn.prepare(
        "SELECT rule_id, plan_digest, authorized_label, context_id, principal_id,
                scope, allow, created_at, created_by, learned_from, revoked_at
         FROM approval_rules
         WHERE plan_digest = ?1 AND revoked_at IS NULL
           AND (scope = 'always' OR (scope = 'session' AND context_id = ?2 AND principal_id = ?3))
         ORDER BY created_at DESC",
    )?;
    let in_scope: Vec<RuleRow> = stmt
        .query_map(params![plan_digest, context_id, principal_id], row_to_rule)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if let Some(matching) = in_scope.iter().find(|r| r.authorized_label == presented_label) {
        return Ok(Some(matching.clone()));
    }
    match in_scope.first() {
        None => Ok(None),
        Some(mismatched) => Err(LedgerError::LabelMismatch {
            plan_digest: plan_digest.to_string(),
            authorized_label: mismatched.authorized_label.clone(),
            presented_label: presented_label.to_string(),
        }),
    }
}

/// Revoke a rule. Idempotent: revoking an already-revoked rule is a no-op
/// success, not an error — only a nonexistent `rule_id` is
/// [`LedgerError::RuleNotFound`].
pub fn revoke(conn: &Connection, rule_id: &str) -> Result<()> {
    let now = now_millis();
    let rows = conn.execute(
        "UPDATE approval_rules SET revoked_at = ?1 WHERE rule_id = ?2 AND revoked_at IS NULL",
        params![now, rule_id],
    )?;
    if rows > 0 {
        return Ok(());
    }
    match get_rule(conn, rule_id)? {
        Some(_) => Ok(()), // already revoked
        None => Err(LedgerError::RuleNotFound(rule_id.to_string())),
    }
}

pub fn get_rule(conn: &Connection, rule_id: &str) -> Result<Option<RuleRow>> {
    conn.query_row(
        "SELECT rule_id, plan_digest, authorized_label, context_id, principal_id,
                scope, allow, created_at, created_by, learned_from, revoked_at
         FROM approval_rules WHERE rule_id = ?1",
        params![rule_id],
        row_to_rule,
    )
    .optional()
    .map_err(LedgerError::from)
}

fn row_to_rule(row: &rusqlite::Row) -> rusqlite::Result<RuleRow> {
    let scope_raw: String = row.get(5)?;
    let allow: i64 = row.get(6)?;
    Ok(RuleRow {
        rule_id: row.get(0)?,
        plan_digest: row.get(1)?,
        authorized_label: row.get(2)?,
        context_id: row.get(3)?,
        principal_id: row.get(4)?,
        scope: parse_enum::<RuleScope>("scope", &scope_raw).map_err(|e| match e {
            LedgerError::Db(inner) => inner,
            other => rusqlite::Error::InvalidColumnType(0, other.to_string(), rusqlite::types::Type::Text),
        })?,
        allow: allow != 0,
        created_at: row.get(7)?,
        created_by: row.get(8)?,
        learned_from: row.get(9)?,
        revoked_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::ask::create_ask;
    use crate::decide::{DecideInput, decide};
    use crate::fixtures::{ask_with_plan, minimal_ask, open_memory};
    use crate::types::VarBinding;

    use super::*;

    fn decided_allowed(conn: &Connection, request_id: &str) {
        decide(conn, request_id, DecideInput { allow: true, decided_by: Some(b"alice"), ..Default::default() }).unwrap();
    }

    #[test]
    fn learn_from_approval_with_a_bound_plan_succeeds() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-bound", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);

        let rule = learn_from_approval(&conn, &request_id, RuleScope::Always, true, Some(b"alice")).unwrap();
        assert_eq!(rule.plan_digest, "digest-bound");
        assert_eq!(rule.authorized_label, "rm target");
        assert_eq!(rule.learned_from.as_deref(), Some(request_id.as_str()));
        assert!(rule.revoked_at.is_none());
    }

    /// Guarantee 3's core test: a plan with a free variable must never
    /// produce a rule, and the error must name exactly which statement and
    /// variable made it unsafe.
    #[test]
    fn learn_from_approval_with_a_free_variable_is_refused() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-free", VarBinding::Free, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);

        let err = learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap_err();
        match err {
            LedgerError::FreeVariableRule { request_id: rid, stmt_seq, var_name } => {
                assert_eq!(rid, request_id);
                assert_eq!(stmt_seq, 0);
                assert_eq!(var_name, "TARGET");
            }
            other => panic!("expected FreeVariableRule, got {other:?}"),
        }

        // And, just as important: no rule row was created.
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM approval_rules", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn learn_from_approval_without_a_plan_is_refused() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        decided_allowed(&conn, &request_id);
        let err = learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap_err();
        assert!(matches!(err, LedgerError::NoPlanRecorded(id) if id == request_id));
    }

    #[test]
    fn learn_from_approval_without_a_label_is_refused() {
        let conn = open_memory();
        let mut ask = ask_with_plan("digest-nolabel", VarBinding::Bound, "unused");
        ask.authorized_label = None;
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        let err = learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap_err();
        assert!(matches!(err, LedgerError::NoLabelRecorded(id) if id == request_id));
    }

    #[test]
    fn redeem_with_no_rule_at_all_is_a_clean_none() {
        let conn = open_memory();
        assert!(redeem(&conn, "no-such-digest", "whatever", None, None).unwrap().is_none());
    }

    #[test]
    fn redeem_with_a_matching_label_succeeds() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-match", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap();

        let rule = redeem(&conn, "digest-match", "rm target", None, None).unwrap();
        assert!(rule.is_some());
        assert_eq!(rule.unwrap().authorized_label, "rm target");
    }

    /// Guarantee 4's core test: a rule exists for this digest, but the
    /// redemption presents a DIFFERENT label — this must be a loud,
    /// distinguishable error, never `Ok(None)` and never `Ok(Some(_))`.
    #[test]
    fn redeem_with_a_mismatched_label_is_a_loud_distinct_error() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-mismatch", VarBinding::Bound, "rm build-artifacts");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap();

        let err = redeem(&conn, "digest-mismatch", "rm /etc", None, None).unwrap_err();
        match err {
            LedgerError::LabelMismatch { plan_digest, authorized_label, presented_label } => {
                assert_eq!(plan_digest, "digest-mismatch");
                assert_eq!(authorized_label, "rm build-artifacts");
                assert_eq!(presented_label, "rm /etc");
            }
            other => panic!("expected LabelMismatch, got {other:?}"),
        }
    }

    #[test]
    fn session_scoped_rule_does_not_match_a_different_context() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-session", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        learn_from_approval(&conn, &request_id, RuleScope::Session, true, None).unwrap();

        // Same label, but a context that never created this rule — must
        // not match (the ask's own fixture context_id is [1,2,3,4]).
        let other_context = vec![9, 8, 7, 6];
        let result = redeem(&conn, "digest-session", "rm target", Some(&other_context), Some(&[9, 9, 9]));
        assert!(result.unwrap().is_none(), "a session rule must not leak to a different context");
    }

    #[test]
    fn session_scoped_rule_matches_its_own_context() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-session-2", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        learn_from_approval(&conn, &request_id, RuleScope::Session, true, None).unwrap();

        // The fixture ask's own context/principal ([1,2,3,4] / [9,9,9]).
        let rule = redeem(&conn, "digest-session-2", "rm target", Some(&[1, 2, 3, 4]), Some(&[9, 9, 9])).unwrap();
        assert!(rule.is_some());
    }

    #[test]
    fn revoked_rule_no_longer_redeems() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-revoke", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        let rule = learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap();

        revoke(&conn, &rule.rule_id).unwrap();
        assert!(redeem(&conn, "digest-revoke", "rm target", None, None).unwrap().is_none());
    }

    #[test]
    fn revoking_twice_is_idempotent() {
        let conn = open_memory();
        let ask = ask_with_plan("digest-revoke-2", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        let rule = learn_from_approval(&conn, &request_id, RuleScope::Always, true, None).unwrap();

        revoke(&conn, &rule.rule_id).unwrap();
        revoke(&conn, &rule.rule_id).unwrap();
    }

    #[test]
    fn revoking_an_unknown_rule_is_not_found() {
        let conn = open_memory();
        assert!(matches!(revoke(&conn, "no-such-rule").unwrap_err(), LedgerError::RuleNotFound(_)));
    }
}
