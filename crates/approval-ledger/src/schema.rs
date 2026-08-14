//! DDL for the approval ledger. `migrate` is the crate's one schema entry
//! point: idempotent (`CREATE TABLE/INDEX/TRIGGER IF NOT EXISTS`), safe to
//! call on every process start, matching `KernelDb`'s approach
//! (`kaijutsu-kernel/src/kernel_db.rs`) so this crate feels native next to
//! it even though it has zero dependency on it.
//!
//! ## Design note: this schema is a *backwards* design, not a forward port
//!
//! An earlier draft mirrored the wire shape one-for-one: every plan-tree
//! table keyed on `request_id`. That duplicates the entire statement/
//! command/arg/redirect/var AST on every single ask, even when the exact
//! same shell text is asked about a hundred times (which is the *common*
//! case — a rule exists precisely to answer "have we seen this exact plan
//! before?"). It also cannot express "a late answer arrived after timeout
//! without silently overwriting `expired`" using one mutable `status` row.
//!
//! This version fixes both, informed by reading kaish 0.14's actual
//! `ast::plan` / `kaish_types::plan` types (not just their names):
//!
//! 1. **The plan tree is re-rooted on `plan_digest`, not `request_id`.**
//!    `PlanDigest` (kaish-types) is a content hash over `rendered` text with
//!    credentials stripped — the plan is an immutable document, produced
//!    once by `plan_program` and never edited. Rooting `approval_plans` (and
//!    everything under it) on that digest makes storing it content-
//!    addressed, exactly like this workspace's own `script_bodies` table
//!    below and `kaijutsu-cas` elsewhere: `create_ask` only inserts the tree
//!    the first time a digest is seen, and every later ask referencing the
//!    same digest just points `approvals.plan_digest` at the existing row.
//!    `approval_plans.has_free_vars` is computed once at that first insert
//!    (from `approval_plan_vars`) and cached, so guarantee 3's trigger
//!    (below) is a cheap point lookup instead of an `EXISTS` scan on every
//!    `BEGIN IMMEDIATE` rule-creation transaction.
//!
//! 2. **`approval_events` is an append-only ledger alongside the mutable
//!    `approvals` snapshot, not instead of it.** `approvals.status` /
//!    `claimed_at` / `decided_at` / … stay as real columns — there is
//!    exactly one row's worth of these per ask, so the write-amplification
//!    argument that justifies de-duplicating the (large, repeated) plan
//!    tree does not apply to them, and the hot "what's pending right now"
//!    query stays a plain indexed `SELECT`, not a "last-event-wins" derived
//!    read. But guarantee 6 explicitly requires a late answer after
//!    timeout to be *recorded*, and a single mutable row cannot represent
//!    both "this is what happened" and "this is what was rejected" —
//!    `approval_events` is where every claim/decide/expire/abandon
//!    *attempt* lands, success or not, so nothing about the ask's real
//!    history is ever lost to a later overwrite. `decide()`/`claim()` write
//!    both: the snapshot (if the attempt wins) and an event row (always).
//!
//! Two things a forward port would not have caught, from reading kaish's
//! real types directly:
//!
//! - `PlannedRedirect::target` is a `PlannedValue`, the exact same
//!   plain/redacted enum as a command argument — so
//!   `approval_plan_redirects` reuses the args table's `value_kind` /
//!   `value_text` / `redact_kind` / `fingerprint` shape instead of a
//!   bespoke `target_kind`/`target_text` pair that would collide in name
//!   with the *redirect operator* kind (`>`, `>>`, `2>`, …).
//! - Free/bound variable analysis in kaish lives at the *statement* level
//!   (`Plan::free_variables` / `bound_variables`), never per-command — so
//!   `approval_plan_vars` is keyed `(plan_digest, stmt_seq, name)` with no
//!   `cmd_seq`, matching the real shape instead of inventing a finer grain
//!   nothing produces.
//!
//! Where this deliberately does **not** follow a second-opinion review
//! (`gemini-pro`, consulted on this exact question before writing this
//! file): args and redirect targets stay in normalized child tables, not
//! flattened to a JSON column on `approval_plan_commands`. That would trade
//! away exactly the two things this schema needs from them — a `CHECK`
//! that a redacted value never carries `value_text`, and a queryable
//! `fingerprint` column for "has this same secret shown up under a
//! different digest" auditing — for a query pattern (regex over a JSON
//! blob) nobody asked for. `feedback_sql_schema.md`'s own stated exception
//! ("JSON only when the relational model adds cost with zero query
//! benefit") does not hold here.

use rusqlite::{Connection, Result as SqliteResult};

/// The DDL, applied in dependency order (a table's `REFERENCES` target must
/// already exist). Every statement is `IF NOT EXISTS` / `CREATE ... IF NOT
/// EXISTS`, so `migrate` is safe to call every process start against an
/// already-migrated database — there is no migration-version table because
/// there is, so far, exactly one schema generation (kernel_db's
/// ALTER-TABLE ladder is the pattern to reach for if that changes).
const DDL: &str = r#"
-- ── Plan documents (content-addressed, immutable, shared) ───────────────
-- A `Plan` (kaish 0.14 `plan_program`) rendered UNEXPANDED: `${HOME}` and
-- `$(...)` stay as written, because the point is to show a human what was
-- *asked*, before anything it names has resolved. `plan_digest` is the
-- caller's content hash over that rendered text with confirmation
-- credentials stripped (kaish's `strip_confirm_tokens` + the caller's own
-- hasher — this crate never computes a digest, only stores and matches
-- the one it's given). One row per distinct digest, regardless of how many
-- `approvals` rows later ask about the identical plan.
CREATE TABLE IF NOT EXISTS approval_plans (
    plan_digest   TEXT    NOT NULL PRIMARY KEY,
    -- Cached at first insert from this plan's own `approval_plan_vars`
    -- rows: 1 iff ANY statement in the plan has a `binding = 'free'`
    -- variable. This is guarantee 3's fast path — the
    -- `approval_rules_reject_free_variable_plans` trigger below reads this
    -- one column instead of re-scanning `approval_plan_vars` on every rule
    -- insert. Never recomputed after insert: the plan is immutable, so the
    -- answer can't change under it.
    has_free_vars INTEGER NOT NULL CHECK (has_free_vars IN (0, 1)),
    created_at    INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- One row per top-level statement in the plan (kaish plans a whole
-- program; a multi-statement ask carries more than one). `rendered` is the
-- unexpanded text; `statement_kind` is kaish's open vocabulary
-- ("command", "pipeline", "for", "and_chain", …) — never CHECK-constrained
-- here because kaish, not this crate, owns that list and adds to it.
CREATE TABLE IF NOT EXISTS approval_plan_statements (
    plan_digest    TEXT    NOT NULL REFERENCES approval_plans(plan_digest),
    stmt_seq       INTEGER NOT NULL,
    rendered       TEXT    NOT NULL,
    statement_kind TEXT    NOT NULL,
    PRIMARY KEY (plan_digest, stmt_seq)
);

-- Every command a statement would run — control-structure bodies, `if`
-- conditions, and `$(...)` substitutions included, because kaish's own
-- `plan_program` doc comment is explicit that each of those is a command
-- the statement runs. `backgrounded` is INTEGER 0/1 (SQLite has no BOOLEAN
-- affinity); matches kaish's `PlannedCommand::background`.
CREATE TABLE IF NOT EXISTS approval_plan_commands (
    plan_digest  TEXT    NOT NULL,
    stmt_seq     INTEGER NOT NULL,
    cmd_seq      INTEGER NOT NULL,
    name         TEXT    NOT NULL,
    backgrounded INTEGER NOT NULL CHECK (backgrounded IN (0, 1)),
    PRIMARY KEY (plan_digest, stmt_seq, cmd_seq),
    FOREIGN KEY (plan_digest, stmt_seq)
        REFERENCES approval_plan_statements(plan_digest, stmt_seq)
);

-- One command's argv, in order. `value_kind` mirrors kaish's
-- `PlannedValue` exactly: `plain` carries the literal text in `value_text`;
-- `redacted` carries no text at all (kaish's own redaction — today only
-- its `--confirm=<key>` flag — never puts the credential in a plan to
-- begin with) and instead names what kind of thing was withheld
-- (`redact_kind`, e.g. `"confirm-key"`) plus an optional stable
-- `fingerprint` so an auditor can ask "same credential as last time?"
-- without ever holding it. The CHECK keeps the two shapes from being
-- storable in an inconsistent half-state (a `redacted` row with leftover
-- `value_text`, or a `plain` row silently missing its text).
CREATE TABLE IF NOT EXISTS approval_plan_args (
    plan_digest  TEXT    NOT NULL,
    stmt_seq     INTEGER NOT NULL,
    cmd_seq      INTEGER NOT NULL,
    arg_seq      INTEGER NOT NULL,
    value_kind   TEXT    NOT NULL CHECK (value_kind IN ('plain', 'redacted')),
    value_text   TEXT,
    redact_kind  TEXT,
    fingerprint  TEXT,
    PRIMARY KEY (plan_digest, stmt_seq, cmd_seq, arg_seq),
    FOREIGN KEY (plan_digest, stmt_seq, cmd_seq)
        REFERENCES approval_plan_commands(plan_digest, stmt_seq, cmd_seq),
    CHECK (
        (value_kind = 'plain'    AND value_text IS NOT NULL AND redact_kind IS NULL)
        OR
        (value_kind = 'redacted' AND value_text IS NULL     AND redact_kind IS NOT NULL)
    )
);

-- A command's redirects (`>`, `>>`, `2>`, `<`, …). `op` is the operator as
-- kaish rendered it — a merge redirect (`2>&1`) has no target, so `op`
-- alone is `2>&1` and the value columns are the `redacted`-with-no-target
-- shape isn't quite right for that case either; callers store a
-- `plain` empty-string target for a merge redirect rather than invent a
-- third value_kind, since kaish's own `PlannedRedirect` always carries a
-- `PlannedValue` target (never `Option`). Target reuses the SAME
-- plain/redacted vocabulary as args (see file header: `PlannedRedirect`'s
-- `target` field is literally a `PlannedValue`, not a bespoke type) rather
-- than the `target_kind`/`target_text` shape an earlier draft of this
-- schema used — that pairing invited confusion between "redirect operator
-- kind" and "value redaction kind".
CREATE TABLE IF NOT EXISTS approval_plan_redirects (
    plan_digest  TEXT    NOT NULL,
    stmt_seq     INTEGER NOT NULL,
    cmd_seq      INTEGER NOT NULL,
    redir_seq    INTEGER NOT NULL,
    op           TEXT    NOT NULL,
    value_kind   TEXT    NOT NULL CHECK (value_kind IN ('plain', 'redacted')),
    value_text   TEXT,
    redact_kind  TEXT,
    fingerprint  TEXT,
    PRIMARY KEY (plan_digest, stmt_seq, cmd_seq, redir_seq),
    FOREIGN KEY (plan_digest, stmt_seq, cmd_seq)
        REFERENCES approval_plan_commands(plan_digest, stmt_seq, cmd_seq),
    CHECK (
        (value_kind = 'plain'    AND value_text IS NOT NULL AND redact_kind IS NULL)
        OR
        (value_kind = 'redacted' AND value_text IS NULL     AND redact_kind IS NOT NULL)
    )
);

-- The statement-level variable analysis (`Plan::free_variables` /
-- `bound_variables` in kaish-types) — NOT per-command, because kaish's own
-- analysis isn't: a name is free or bound for the whole statement it
-- appears in. THIS is the table guarantee 3 reads: any row here with
-- `binding = 'free'` for a plan's statement means that plan must never
-- back a standing allow-always rule (`rm "$TARGET"` — the plan is
-- pre-resolution by design, so the digest can never be made safe by
-- resolving it later). `approval_plans.has_free_vars` is the cached
-- OR-reduction of this table, computed once when the plan is first
-- inserted.
CREATE TABLE IF NOT EXISTS approval_plan_vars (
    plan_digest TEXT NOT NULL,
    stmt_seq    INTEGER NOT NULL,
    name        TEXT NOT NULL,
    binding     TEXT NOT NULL CHECK (binding IN ('free', 'bound')),
    PRIMARY KEY (plan_digest, stmt_seq, name),
    FOREIGN KEY (plan_digest, stmt_seq)
        REFERENCES approval_plan_statements(plan_digest, stmt_seq)
);

-- ── Approvals (the ask) ───────────────────────────────────────────────
-- One row per ask, inserted and COMMITTED before any human is ever
-- prompted (guarantee 1 — durable before asked) — a crash between this
-- insert and showing the prompt leaves the row `pending`, never lost.
-- Deliberately NO foreign key on `context_id`: an audit row must outlive
-- the context that spawned it (contexts get archived/wiped; this ledger
-- does not — "retained forever with timestamps for later windowing").
-- `plan_digest` / `rc_run_id` ARE real foreign keys because their targets
-- (`approval_plans`, `rc_runs`) share this crate's own never-pruned
-- retention policy, so there's no equivalent outlive-the-parent hazard.
-- `status` plus its own timestamp/actor columns stay a plain mutable
-- snapshot (not folded into `approval_events`) because there is exactly
-- one of each per ask — no duplication to fight — and the hottest query
-- this ledger serves ("what's pending right now") wants a direct indexed
-- read, not a last-event-wins derivation. `approval_events` below carries
-- the history a single mutable row cannot: every claim/decide/expire/
-- abandon *attempt*, including the ones this table's own immutability
-- trigger rejects.
CREATE TABLE IF NOT EXISTS approvals (
    request_id       TEXT    NOT NULL PRIMARY KEY,
    context_id       BLOB    NOT NULL,
    principal_id     BLOB    NOT NULL,
    origin           TEXT    NOT NULL CHECK (origin IN ('hook', 'shell_gate', 'kj_verb')),
    instance         TEXT,
    tool             TEXT,
    hook_id          TEXT,
    description      TEXT    NOT NULL,
    plan_digest      TEXT    REFERENCES approval_plans(plan_digest),
    -- The label-not-id anchor (guarantee 4): what this ask NAMED, not an id
    -- that could later resolve elsewhere. NULL is legal here (not every
    -- ask names a single reusable thing) but NOT NULL on `approval_rules`
    -- below — a rule with nothing to compare a redemption's label against
    -- would be a guard that can never fail, which is worse than no rule.
    authorized_label TEXT,
    rc_run_id        TEXT    REFERENCES rc_runs(run_id),
    status           TEXT    NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'claimed', 'allowed', 'denied', 'expired', 'abandoned')),
    created_at       INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    -- Nullable on purpose: whether (and how long until) an ask auto-expires
    -- is caller policy, not something this storage crate should force —
    -- but a NULL here means nothing ever fails this ask closed on a clock,
    -- so a caller that wants guarantee 2's timeout leg must set it, or own
    -- calling `abandon()` some other way.
    expires_at       INTEGER,
    claimed_at       INTEGER,
    claimed_by       BLOB,
    decided_at       INTEGER,
    decided_by       BLOB,
    decided_option   TEXT,
    remember_scope   TEXT,
    auto_reason      TEXT
);
CREATE INDEX IF NOT EXISTS idx_approvals_status_created
    ON approvals(status, created_at);
CREATE INDEX IF NOT EXISTS idx_approvals_context_created
    ON approvals(context_id, created_at);
CREATE INDEX IF NOT EXISTS idx_approvals_plan_digest
    ON approvals(plan_digest);

-- A decided ask's status is a one-way ratchet into a terminal state
-- (guarantee 6). This is enforced at the single Rust write site
-- (`decide`/`expire`/`abandon` all use `UPDATE ... WHERE status IN
-- ('pending','claimed') ... RETURNING`, so a losing attempt affects zero
-- rows and never reaches this trigger at all) — but the trigger is the
-- backstop for any OTHER write path, present or future, that forgets that
-- WHERE clause: it turns a silent lost-update into a loud SQLite error
-- instead of a quietly overwritten `expired`/`denied`.
CREATE TRIGGER IF NOT EXISTS approvals_decided_is_immutable
BEFORE UPDATE OF status ON approvals
FOR EACH ROW WHEN OLD.status IN ('allowed', 'denied', 'expired', 'abandoned')
BEGIN
    SELECT RAISE(ABORT, 'approval already reached a terminal status; re-deciding is refused (guarantee 6)');
END;

-- ── Approval options ─────────────────────────────────────────────────
-- The choices offered to the human (e.g. allow-once / allow-always /
-- deny), in presentation order. Wholly owned by one ask — CASCADE is safe
-- (nothing else references these rows).
CREATE TABLE IF NOT EXISTS approval_options (
    request_id TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    option_id  TEXT    NOT NULL,
    label      TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    PRIMARY KEY (request_id, seq)
);

-- ── Approval signals ─────────────────────────────────────────────────
-- Advisory annotations attached to an ask (a rule that almost matched, a
-- classifier's risk score on one command) — informational, never itself a
-- gate. `stmt_seq`/`cmd_seq` are nullable: a signal can speak to the whole
-- ask rather than one command within it.
CREATE TABLE IF NOT EXISTS approval_signals (
    request_id  TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    source_kind TEXT    NOT NULL CHECK (source_kind IN ('rule', 'classifier')),
    source_id   TEXT,
    model_id    TEXT,
    weight_hash TEXT,
    stmt_seq    INTEGER,
    cmd_seq     INTEGER,
    label       TEXT,
    score       REAL,
    verdict     TEXT    NOT NULL CHECK (verdict IN ('escalate', 'deny', 'allow')),
    PRIMARY KEY (request_id, seq)
);

-- ── Approval events (the append-only ledger) ────────────────────────
-- Every claim/decide/expire/abandon ATTEMPT, whether it won or lost —
-- this is what makes guarantee 6's "a late answer after timeout is
-- recorded, never silently dropped or silently applied" representable at
-- all. A single mutable `status` column can hold only the CURRENT truth;
-- this table holds every truth that was ever attempted against it. `kind
-- = 'late_decision'` is the specific row a rejected post-terminal decide()
-- writes — never a `decided` row, because it never became the outcome.
CREATE TABLE IF NOT EXISTS approval_events (
    request_id     TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    kind           TEXT    NOT NULL
        CHECK (kind IN ('claimed', 'decided', 'expired', 'abandoned', 'late_decision')),
    actor          BLOB,
    decided_option TEXT,
    remember_scope TEXT,
    auto_reason    TEXT,
    note           TEXT,
    created_at     INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (request_id, seq)
);

-- ── Approval rules (standing "remember this" policy) ────────────────
-- A rule generalized from one decided approval (`learned_from`), matched
-- by (`plan_digest`, `authorized_label`) against FUTURE asks. Deliberately
-- its own table, not a wider-scope row in `approvals`: a rule has no
-- options list, no claim/decide lifecycle, no single originating plan
-- tree of its own (many future asks with different `request_id`s can
-- match the same rule) — folding it into `approvals` would mean every
-- approvals row grows nullable rule-only columns and every rule grows
-- nullable ask-only columns, for two things that are read by completely
-- different queries (the human-prompt queue vs. the redemption
-- fast-path). `learned_from` intentionally has NO cascade: the rule is
-- meant to outlive the one ask that spawned it.
--
-- `plan_digest` and `authorized_label` are BOTH `NOT NULL` — tighter than
-- `approvals`, on purpose: a rule keyed on an unknown digest or an unnamed
-- label wouldn't be a guard that can fail closed, it would be a guard that
-- can never fire at all, which is a worse silent failure mode than
-- refusing to create it.
CREATE TABLE IF NOT EXISTS approval_rules (
    rule_id          TEXT    NOT NULL PRIMARY KEY,
    plan_digest      TEXT    NOT NULL REFERENCES approval_plans(plan_digest),
    authorized_label TEXT    NOT NULL,
    context_id       BLOB,
    principal_id     BLOB,
    scope            TEXT    NOT NULL CHECK (scope IN ('session', 'always')),
    allow            INTEGER NOT NULL CHECK (allow IN (0, 1)),
    created_at       INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by       BLOB,
    learned_from     TEXT    REFERENCES approvals(request_id),
    revoked_at       INTEGER
);
-- The redemption fast-path: "is there a live rule for this digest+label?"
-- Partial (WHERE revoked_at IS NULL) so a revoked rule never shadows —or
-- gets confused with— a later live one keyed the same way.
CREATE INDEX IF NOT EXISTS idx_approval_rules_active
    ON approval_rules(plan_digest, authorized_label) WHERE revoked_at IS NULL;

-- Guarantee 3's schema-level backstop: refuse the INSERT outright when the
-- referenced plan is known to carry a free variable, or when the plan is
-- NOT known at all. The `COALESCE(..., 1)` is deliberate — a missing
-- `approval_plans` row (e.g. `PRAGMA foreign_keys` was left off, so the
-- `REFERENCES` above didn't actually block it) must fail CLOSED exactly
-- like an unrecognized status must (guarantee 2), not fall through as "no
-- free vars found, so allow it". The single Rust write site
-- (`rules::learn_from_approval`) checks this first and returns a specific,
-- readable `FreeVariableRule` error naming the offending statement and
-- variable; this trigger is what still stops it if a future write path
-- ever bypasses that function.
CREATE TRIGGER IF NOT EXISTS approval_rules_reject_free_variable_plans
BEFORE INSERT ON approval_rules
FOR EACH ROW WHEN COALESCE(
    (SELECT has_free_vars FROM approval_plans WHERE plan_digest = NEW.plan_digest), 1
) = 1
BEGIN
    SELECT RAISE(ABORT, 'refusing an allow-always rule: plan has a free variable, or its digest is unrecorded (guarantee 3, fail-closed)');
END;

-- ── rc run log (the "checklist of what ran") ────────────────────────
-- One row per rc-lifecycle invocation (create/fork/drift/…) for a
-- context. This is what would have made a silently-inert startup rc
-- sweep visible on run one instead of by incident review — a durable,
-- queryable record of which scripts actually fired, not just that the
-- lifecycle verb was called.
CREATE TABLE IF NOT EXISTS rc_runs (
    run_id       TEXT    NOT NULL PRIMARY KEY,
    context_id   BLOB    NOT NULL,
    context_type TEXT    NOT NULL,
    verb         TEXT    NOT NULL,
    started_at   INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    finished_at  INTEGER,
    outcome      TEXT CHECK (outcome IS NULL OR outcome IN ('ok', 'failed', 'abandoned'))
);
CREATE INDEX IF NOT EXISTS idx_rc_runs_context_started
    ON rc_runs(context_id, started_at);

-- One row per script the run executed, in order, pointing at its
-- content-addressed body. CASCADE is correct here (unlike `approval_plan_*`
-- above): a run's script log is wholly owned by that run, not shared
-- across runs the way a plan can be shared across asks.
CREATE TABLE IF NOT EXISTS rc_run_scripts (
    run_id      TEXT    NOT NULL REFERENCES rc_runs(run_id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    path        TEXT    NOT NULL,
    body_sha256 TEXT    NOT NULL REFERENCES script_bodies(sha256),
    exit_code   INTEGER,
    started_at  INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    finished_at INTEGER,
    PRIMARY KEY (run_id, seq)
);

-- Content-addressed rc script bodies (same pattern as `hook_scripts` in
-- kernel_db.rs and `approval_plans` above): identical script text across
-- many runs stores once. No cascade target — a body must survive even a
-- run row's own (never-expected-in-practice) deletion, since other runs
-- may share it.
CREATE TABLE IF NOT EXISTS script_bodies (
    sha256       TEXT    NOT NULL PRIMARY KEY,
    body         TEXT    NOT NULL,
    first_seen_at INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);
"#;

/// Create every table/index/trigger this crate owns, idempotently. Safe to
/// call on every process start. Does not touch `PRAGMA foreign_keys` —
/// that is the caller's connection-wide setting to own (see the crate
/// docs); this crate's own invariants (guarantees 2, 3, 6) are enforced by
/// `CHECK`s and triggers that do not depend on FK enforcement being on.
pub fn migrate(conn: &Connection) -> SqliteResult<()> {
    conn.execute_batch(DDL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn every_table_this_module_claims_actually_exists() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for table in [
            "approval_plans",
            "approval_plan_statements",
            "approval_plan_commands",
            "approval_plan_args",
            "approval_plan_redirects",
            "approval_plan_vars",
            "approvals",
            "approval_options",
            "approval_signals",
            "approval_events",
            "approval_rules",
            "rc_runs",
            "rc_run_scripts",
            "script_bodies",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} must exist after migrate()");
        }
    }

    /// A direct sanity check on the `CHECK` constraints, independent of any
    /// Rust-level enum validation — this is what actually stops a bad
    /// `status` from ever being written, regardless of what inserted it.
    #[test]
    fn status_check_constraint_rejects_an_unrecognized_value() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description)
             VALUES ('r1', X'01', X'02', 'shell_gate', 'x')",
            [],
        )
        .unwrap();
        let err = conn
            .execute("UPDATE approvals SET status = 'sideways' WHERE request_id = 'r1'", [])
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "expected a CHECK violation, got: {err}");
    }
}
