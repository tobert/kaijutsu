//! Standing "remember this" rules — generalizing one statement of a
//! decided approval into a rule that can auto-decide future asks'
//! statements (guarantees 3 and 4), and composing a whole ask's coverage
//! from those per-statement rules (Amy's 2026-08-14 ruling — see
//! `schema.rs` header: generalization granularity is the point, so
//! coverage is checked and composed per statement, never per ask).
//!
//! [`learn_from_approval`] is guarantee 3's single write site: it is the
//! only function in this crate that inserts into `approval_rules`, and it
//! refuses — with [`LedgerError::FreeVariableRule`] naming the offending
//! variable — when the targeted statement has any `binding = 'free'`
//! variable AND the rule being created is an `allow` rule. A `deny` rule
//! for the same free-variable statement is permitted (a standing deny is
//! strictly safety-increasing).
//!
//! [`redeem`] is guarantee 4: it never conflates "no rule covers this
//! statement" with "a rule covers this statement but not what you
//! presented" — at every position in the statement set, not just the
//! first.

use rusqlite::{Connection, OptionalExtension, params};

use crate::ask::get_approval;
use crate::error::{LedgerError, Result};
use crate::time::now_millis;
use crate::types::{AskCoverage, RuleRow, RuleScope, StatementVerdict, parse_enum};

/// Generalize one statement of a decided approval (`request_id`,
/// `stmt_seq` into `approval_ask_statements`) into a standing rule.
/// `rule_id` is generated (UUIDv7). Fails with [`LedgerError::NotFound`]
/// if `request_id` doesn't exist, [`LedgerError::NoSuchStatement`] if
/// `stmt_seq` isn't one of its statements (including an ask with no
/// statements at all — a `kj_verb`-origin ask has nothing to generalize
/// through this path), [`LedgerError::NoLabelRecorded`] if it has no
/// `authorized_label`, and — the guarantee this function exists to
/// protect — [`LedgerError::FreeVariableRule`] if the targeted statement
/// reads an unresolved variable AND `allow` is `true`. None of these leave
/// a partial row: every check runs before the `INSERT`.
pub fn learn_from_approval(
    conn: &Connection,
    request_id: &str,
    stmt_seq: i64,
    scope: RuleScope,
    allow: bool,
    created_by: Option<&[u8]>,
) -> Result<RuleRow> {
    let approval = get_approval(conn, request_id)?.ok_or_else(|| LedgerError::NotFound(request_id.to_string()))?;
    let authorized_label = approval
        .authorized_label
        .clone()
        .ok_or_else(|| LedgerError::NoLabelRecorded(request_id.to_string()))?;

    let statement_digest: Option<String> = conn
        .query_row(
            "SELECT statement_digest FROM approval_ask_statements WHERE request_id = ?1 AND stmt_seq = ?2",
            params![request_id, stmt_seq],
            |row| row.get(0),
        )
        .optional()?;
    let statement_digest = statement_digest
        .ok_or_else(|| LedgerError::NoSuchStatement { request_id: request_id.to_string(), stmt_seq })?;

    // The authoritative check (guarantee 3): query the statement's own
    // variable rows directly rather than trusting
    // `approval_statements.has_free_vars` — that cache exists to make the
    // SCHEMA trigger cheap, not to be this function's source of truth.
    // Only gates `allow` rules — same asymmetry as the trigger in
    // `schema.rs`: a deny rule for a free-variable statement is strictly
    // safety-increasing, so it is never refused here.
    if allow {
        let offender: Option<String> = conn
            .query_row(
                "SELECT name FROM approval_statement_vars
                 WHERE statement_digest = ?1 AND binding = 'free'
                 ORDER BY name LIMIT 1",
                params![statement_digest],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(var_name) = offender {
            return Err(LedgerError::FreeVariableRule {
                request_id: request_id.to_string(),
                stmt_seq,
                var_name,
            });
        }
    }

    let rule_id = uuid::Uuid::now_v7().to_string();
    let now = now_millis();
    conn.execute(
        "INSERT INTO approval_rules (
            rule_id, statement_digest, authorized_label, context_id, principal_id,
            scope, allow, created_at, created_by, learned_from
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            rule_id,
            statement_digest,
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

/// Check a set of statement digests (an ask's ordered statement list, or
/// any other set a caller wants to probe) against active rules for
/// `presented_label`, and return the per-statement coverage
/// ([`AskCoverage::verdict`] composes it: deny wins outright, allow needs
/// every statement covered, anything else escalates).
///
/// Fails loudly with [`LedgerError::LabelMismatch`] — and stops processing
/// immediately, not just for that one statement — the instant ANY
/// statement in the set is covered by a rule for a DIFFERENT label
/// (guarantee 4). This is never folded into `Uncovered`: a scope
/// violation anywhere in the set must be loud, not indistinguishable from
/// an ordinary cache-miss.
pub fn redeem(
    conn: &Connection,
    statement_digests: &[&str],
    presented_label: &str,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
) -> Result<AskCoverage> {
    let mut per_statement = Vec::with_capacity(statement_digests.len());
    for digest in statement_digests {
        per_statement.push(redeem_one(conn, digest, presented_label, context_id, principal_id)?);
    }
    Ok(AskCoverage { per_statement })
}

/// One statement's worth of [`redeem`] — three distinguishable outcomes
/// (guarantee 4):
/// - `Uncovered` — no rule at all covers this statement+scope. A clean
///   cache-miss; ask a human.
/// - `Allow`/`Deny(rule)` — a rule covers it AND its `authorized_label`
///   matches `presented_label`.
/// - `Err(LabelMismatch)` — a rule covers this statement+scope but
///   authorizes a *different* label than what's being presented now.
fn redeem_one(
    conn: &Connection,
    statement_digest: &str,
    presented_label: &str,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
) -> Result<StatementVerdict> {
    let mut stmt = conn.prepare(
        "SELECT rule_id, statement_digest, authorized_label, context_id, principal_id,
                scope, allow, created_at, created_by, learned_from, revoked_at
         FROM approval_rules
         WHERE statement_digest = ?1 AND revoked_at IS NULL
           AND (scope = 'always' OR (scope = 'session' AND context_id = ?2 AND principal_id = ?3))
         ORDER BY created_at DESC",
    )?;
    let in_scope: Vec<RuleRow> = stmt
        .query_map(params![statement_digest, context_id, principal_id], row_to_rule)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if let Some(matching) = in_scope.iter().find(|r| r.authorized_label == presented_label) {
        return Ok(if matching.allow {
            StatementVerdict::Allow(matching.clone())
        } else {
            StatementVerdict::Deny(matching.clone())
        });
    }
    match in_scope.first() {
        None => Ok(StatementVerdict::Uncovered),
        Some(mismatched) => Err(LedgerError::LabelMismatch {
            statement_digest: statement_digest.to_string(),
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
        "SELECT rule_id, statement_digest, authorized_label, context_id, principal_id,
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
        statement_digest: row.get(1)?,
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
    use crate::fixtures::{ask_with_statement, minimal_ask, open_memory};
    use crate::types::{AskVerdict, VarBinding};

    use super::*;

    fn decided_allowed(conn: &Connection, request_id: &str) {
        decide(conn, request_id, DecideInput { allow: true, decided_by: Some(b"alice"), ..Default::default() }).unwrap();
    }

    #[test]
    fn learn_from_approval_with_a_bound_statement_succeeds() {
        let conn = open_memory();
        let ask = ask_with_statement("digest-bound", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);

        let rule = learn_from_approval(&conn, &request_id, 0, RuleScope::Always, true, Some(b"alice")).unwrap();
        assert_eq!(rule.statement_digest, "digest-bound");
        assert_eq!(rule.authorized_label, "rm target");
        assert_eq!(rule.learned_from.as_deref(), Some(request_id.as_str()));
        assert!(rule.revoked_at.is_none());
    }

    /// Guarantee 3's core test: a statement with a free variable must
    /// never produce an ALLOW rule, and the error must name exactly which
    /// statement position and variable made it unsafe.
    #[test]
    fn learn_from_approval_with_a_free_variable_refuses_an_allow_rule() {
        let conn = open_memory();
        let ask = ask_with_statement("digest-free", VarBinding::Free, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);

        let err = learn_from_approval(&conn, &request_id, 0, RuleScope::Always, true, None).unwrap_err();
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

    /// The other filed item, fixed here: a DENY rule for a free-variable
    /// statement has no safety argument against it and must be permitted.
    #[test]
    fn learn_from_approval_with_a_free_variable_permits_a_deny_rule() {
        let conn = open_memory();
        let ask = ask_with_statement("digest-free-deny", VarBinding::Free, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);

        let rule = learn_from_approval(&conn, &request_id, 0, RuleScope::Always, false, None)
            .expect("a deny rule for a free-variable statement must be permitted");
        assert!(!rule.allow);
        assert_eq!(rule.statement_digest, "digest-free-deny");
    }

    #[test]
    fn learn_from_approval_at_an_out_of_range_position_is_refused() {
        let conn = open_memory();
        let ask = ask_with_statement("digest-range", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        let err = learn_from_approval(&conn, &request_id, 1, RuleScope::Always, true, None).unwrap_err();
        assert!(matches!(err, LedgerError::NoSuchStatement { stmt_seq: 1, .. }));
    }

    #[test]
    fn learn_from_approval_without_any_statements_is_refused() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        decided_allowed(&conn, &request_id);
        let err = learn_from_approval(&conn, &request_id, 0, RuleScope::Always, true, None).unwrap_err();
        assert!(matches!(err, LedgerError::NoSuchStatement { stmt_seq: 0, .. }));
    }

    #[test]
    fn learn_from_approval_without_a_label_is_refused() {
        let conn = open_memory();
        let mut ask = ask_with_statement("digest-nolabel", VarBinding::Bound, "unused");
        ask.authorized_label = None;
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        let err = learn_from_approval(&conn, &request_id, 0, RuleScope::Always, true, None).unwrap_err();
        assert!(matches!(err, LedgerError::NoLabelRecorded(id) if id == request_id));
    }

    #[test]
    fn redeem_with_no_rule_at_all_is_a_clean_uncovered_and_escalates() {
        let conn = open_memory();
        let coverage = redeem(&conn, &["no-such-digest"], "whatever", None, None).unwrap();
        assert!(matches!(coverage.per_statement[0], StatementVerdict::Uncovered));
        assert_eq!(coverage.verdict(), AskVerdict::Escalate);
    }

    fn make_rule(conn: &Connection, digest: &str, binding: VarBinding, label: &str, allow: bool) -> RuleRow {
        let ask = ask_with_statement(digest, binding, label);
        let request_id = create_ask(conn, &ask).unwrap();
        decided_allowed(conn, &request_id);
        learn_from_approval(conn, &request_id, 0, RuleScope::Always, allow, None).unwrap()
    }

    /// The four required coverage compositions.
    #[test]
    fn all_statements_allowed_composes_to_allow() {
        let conn = open_memory();
        make_rule(&conn, "d1", VarBinding::Bound, "ok", true);
        make_rule(&conn, "d2", VarBinding::Bound, "ok", true);

        let coverage = redeem(&conn, &["d1", "d2"], "ok", None, None).unwrap();
        assert_eq!(coverage.verdict(), AskVerdict::Allow);
    }

    #[test]
    fn any_denied_statement_composes_to_deny_even_alongside_an_allowed_one() {
        let conn = open_memory();
        make_rule(&conn, "d1", VarBinding::Bound, "ok", true);
        make_rule(&conn, "d2", VarBinding::Bound, "ok", false);

        let coverage = redeem(&conn, &["d1", "d2"], "ok", None, None).unwrap();
        assert_eq!(coverage.verdict(), AskVerdict::Deny, "a single denied statement must deny the whole ask");
    }

    #[test]
    fn partial_coverage_escalates_never_partially_applies() {
        let conn = open_memory();
        make_rule(&conn, "d1", VarBinding::Bound, "ok", true);
        // d2 has no rule at all.

        let coverage = redeem(&conn, &["d1", "d2"], "ok", None, None).unwrap();
        assert_eq!(coverage.verdict(), AskVerdict::Escalate);
    }

    #[test]
    fn no_coverage_at_all_escalates() {
        let conn = open_memory();
        let coverage = redeem(&conn, &["d1", "d2"], "ok", None, None).unwrap();
        assert_eq!(coverage.verdict(), AskVerdict::Escalate);
    }

    #[test]
    fn an_empty_statement_set_escalates_rather_than_vacuously_allowing() {
        let conn = open_memory();
        let coverage = redeem(&conn, &[], "ok", None, None).unwrap();
        assert_eq!(coverage.verdict(), AskVerdict::Escalate);
    }

    #[test]
    fn redeem_with_a_matching_label_succeeds() {
        let conn = open_memory();
        make_rule(&conn, "digest-match", VarBinding::Bound, "rm target", true);

        let coverage = redeem(&conn, &["digest-match"], "rm target", None, None).unwrap();
        assert!(matches!(&coverage.per_statement[0], StatementVerdict::Allow(rule) if rule.authorized_label == "rm target"));
    }

    /// Guarantee 4's core test, preserved at set granularity: a rule
    /// exists for one of the digests, but the redemption presents a
    /// DIFFERENT label — this must be a loud, distinguishable error for
    /// the whole call, never `Uncovered` and never a silently composed
    /// verdict.
    #[test]
    fn redeem_with_a_mismatched_label_is_a_loud_distinct_error() {
        let conn = open_memory();
        make_rule(&conn, "digest-mismatch", VarBinding::Bound, "rm build-artifacts", true);

        let err = redeem(&conn, &["digest-mismatch"], "rm /etc", None, None).unwrap_err();
        match err {
            LedgerError::LabelMismatch { statement_digest, authorized_label, presented_label } => {
                assert_eq!(statement_digest, "digest-mismatch");
                assert_eq!(authorized_label, "rm build-artifacts");
                assert_eq!(presented_label, "rm /etc");
            }
            other => panic!("expected LabelMismatch, got {other:?}"),
        }
    }

    #[test]
    fn session_scoped_rule_does_not_match_a_different_context() {
        let conn = open_memory();
        let ask = ask_with_statement("digest-session", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        learn_from_approval(&conn, &request_id, 0, RuleScope::Session, true, None).unwrap();

        // Same label, but a context that never created this rule — must
        // not match (the ask's own fixture context_id is [1,2,3,4]).
        let other_context = vec![9, 8, 7, 6];
        let coverage = redeem(&conn, &["digest-session"], "rm target", Some(&other_context), Some(&[9, 9, 9])).unwrap();
        assert!(
            matches!(coverage.per_statement[0], StatementVerdict::Uncovered),
            "a session rule must not leak to a different context"
        );
    }

    #[test]
    fn session_scoped_rule_matches_its_own_context() {
        let conn = open_memory();
        let ask = ask_with_statement("digest-session-2", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        decided_allowed(&conn, &request_id);
        learn_from_approval(&conn, &request_id, 0, RuleScope::Session, true, None).unwrap();

        // The fixture ask's own context/principal ([1,2,3,4] / [9,9,9]).
        let coverage = redeem(&conn, &["digest-session-2"], "rm target", Some(&[1, 2, 3, 4]), Some(&[9, 9, 9])).unwrap();
        assert!(matches!(coverage.per_statement[0], StatementVerdict::Allow(_)));
    }

    #[test]
    fn revoked_rule_no_longer_redeems() {
        let conn = open_memory();
        let rule = make_rule(&conn, "digest-revoke", VarBinding::Bound, "rm target", true);

        revoke(&conn, &rule.rule_id).unwrap();
        let coverage = redeem(&conn, &["digest-revoke"], "rm target", None, None).unwrap();
        assert!(matches!(coverage.per_statement[0], StatementVerdict::Uncovered));
    }

    #[test]
    fn revoking_twice_is_idempotent() {
        let conn = open_memory();
        let rule = make_rule(&conn, "digest-revoke-2", VarBinding::Bound, "rm target", true);
        revoke(&conn, &rule.rule_id).unwrap();
        revoke(&conn, &rule.rule_id).unwrap();
    }

    #[test]
    fn revoking_an_unknown_rule_is_not_found() {
        let conn = open_memory();
        assert!(matches!(revoke(&conn, "no-such-rule").unwrap_err(), LedgerError::RuleNotFound(_)));
    }
}
