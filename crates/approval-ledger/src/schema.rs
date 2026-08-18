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
//! case — a rule exists precisely to answer "have we seen this exact
//! statement before?"). It also cannot express "a late answer arrived
//! after timeout without silently overwriting `expired`" using one
//! mutable `status` row. Both are fixed below.
//!
//! ## The content-addressed unit is the STATEMENT, not the ask (2026-08-14)
//!
//! Amy's ruling, carried here because it explains the whole shape:
//! *"that's why we have lfm2d in the works, to reduce that fatigue while
//! providing some adaptable guardrails."* Generalization granularity IS
//! the point of a rule — a digest keyed on a whole (possibly
//! multi-statement) ask only ever matches a verbatim repeat of that exact
//! ask (`ls` and `ls; pwd` are unrelated asks under that scheme), which is
//! allowlist fatigue in a new costume. A digest keyed on one statement
//! generalizes at command-shape granularity (QwenPaw's
//! ASK→approve→**generalize**), and the classifier this crate doesn't
//! build yet exists to make THAT safe, not to make prompting bearable.
//!
//! This also aligns the schema with what kaish's own source already gives
//! us, not just its names: `plan_program` returns `Vec<PlannedStatement>`,
//! and `Plan::free_variables`/`bound_variables` are per-statement — there
//! was never a coherent "whole program" digest to begin with, only a
//! per-statement one (kaish-types' `PlanDigest` doc comment: a hash over
//! ONE statement's rendered text).
//!
//! - `approval_statements` is the content-addressed root — one row per
//!   distinct `statement_digest`, carrying `rendered` / `statement_kind` /
//!   `has_free_vars` directly. This collapses the earlier two-table split
//!   (a `approval_plans` root plus a child keyed `(plan_digest, stmt_seq)`)
//!   that only existed because an ask-level digest could carry more than
//!   one statement; a statement-level digest never does — a statement IS
//!   its digest now, one row, not a row-per-(digest, position).
//! - `approval_statement_commands` / `_args` / `_redirects` / `_vars` hang
//!   off `statement_digest` alone — no `stmt_seq` in any of their keys.
//!   `stmt_seq` belonged to the old ask-level rooting; it has no meaning
//!   inside a statement's own timeless identity.
//! - `approval_ask_statements` is the new many-to-many join carrying an
//!   ask's ORDERED statement list: `(request_id, stmt_seq) →
//!   statement_digest`. This is where order lives now — the statement
//!   content itself has none, the same way a paragraph doesn't know which
//!   page it's quoted on.
//! - `approvals.plan_digest` is GONE, and deliberately not replaced by a
//!   cached ask-level digest: that would be a second source of truth for
//!   "what was asked" alongside `approval_ask_statements`, and a value
//!   that can only ever be re-derived from another table is exactly the
//!   kind of thing that drifts. Read an ask's statement set through the
//!   join table (`ask::load_ask_statements`).
//! - `approval_rules.plan_digest` → `statement_digest`: a rule is now a
//!   standing verdict on ONE statement shape. An ask composed of several
//!   statements is auto-decidable only if EVERY one of them is covered by
//!   an active rule (`rules::redeem` / `AskVerdict`) — any statement
//!   covered by a `deny` rule denies the whole ask outright; a partially
//!   covered ask is never partially applied, it escalates.
//!
//! Nothing was deployed when this landed — no callers, no data anywhere —
//! so this is a straight schema change: no migration ladder, no
//! compatibility shim, written as if it had always been this shape.
//!
//! Two things a forward port would not have caught, from reading kaish's
//! real types directly:
//!
//! - `PlannedRedirect::target` is a `PlannedValue`, the exact same
//!   plain/redacted enum as a command argument — so
//!   `approval_statement_redirects` reuses the args table's `value_kind` /
//!   `value_text` / `redact_kind` / `fingerprint` shape instead of a
//!   bespoke `target_kind`/`target_text` pair that would collide in name
//!   with the *redirect operator* kind (`>`, `>>`, `2>`, …).
//! - Free/bound variable analysis in kaish lives at the *statement* level
//!   (`Plan::free_variables` / `bound_variables`), never per-command — so
//!   `approval_statement_vars` is keyed `(statement_digest, name)` with no
//!   `cmd_seq`, matching the real shape instead of inventing a finer grain
//!   nothing produces.
//!
//! Where this deliberately does **not** follow a second-opinion review
//! (`gemini-pro`, consulted on this exact question before the schema was
//! first written): args and redirect targets stay in normalized child
//! tables, not flattened to a JSON column on `approval_statement_commands`.
//! That would trade away exactly the two things this schema needs from
//! them — a `CHECK` that a redacted value never carries `value_text`, and
//! a queryable `fingerprint` column for "has this same secret shown up
//! under a different digest" auditing — for a query pattern (regex over a
//! JSON blob) nobody asked for. `feedback_sql_schema.md`'s own stated
//! exception ("JSON only when the relational model adds cost with zero
//! query benefit") does not hold here.
//!
//! ## `approval_events` is an append-only ledger alongside the mutable
//! `approvals` snapshot, not instead of it
//!
//! `approvals.status` / `claimed_at` / `decided_at` / … stay as real
//! columns — there is exactly one row's worth of these per ask, so the
//! write-amplification argument that justifies de-duplicating the (large,
//! repeated) statement tree does not apply to them, and the hot "what's
//! pending right now" query stays a plain indexed `SELECT`, not a
//! "last-event-wins" derived read. But guarantee 6 explicitly requires a
//! late answer after timeout to be *recorded*, and a single mutable row
//! cannot represent both "this is what happened" and "this is what was
//! rejected" — `approval_events` is where every claim/decide/expire/
//! abandon *attempt* lands, success or not, so nothing about the ask's
//! real history is ever lost to a later overwrite. `decide()`/`claim()`
//! write both: the snapshot (if the attempt wins) and an event row
//! (always).

use rusqlite::{Connection, Result as SqliteResult};

/// The DDL, applied in dependency order (a table's `REFERENCES` target must
/// already exist). Every statement is `IF NOT EXISTS` / `CREATE ... IF NOT
/// EXISTS`, so `migrate` is safe to call every process start against an
/// already-migrated database — there is no migration-version table because
/// there is, so far, exactly one schema generation (kernel_db's
/// ALTER-TABLE ladder is the pattern to reach for if that changes).
const DDL: &str = r#"
-- ── Statement documents (content-addressed, immutable, shared) ─────────
-- A `PlannedStatement` (kaish 0.14 `plan_program` returns one per
-- top-level statement) rendered UNEXPANDED: `${HOME}` and `$(...)` stay
-- as written, because the point is to show a human what was *asked*,
-- before anything it names has resolved. `statement_digest` is the
-- caller's content hash over that rendered text with confirmation
-- credentials stripped (kaish's `strip_confirm_tokens` + the caller's own
-- hasher — this crate never computes a digest, only stores and matches
-- the one it's given). One row per distinct digest, regardless of how
-- many asks (or how many statements within one ask) reference it —
-- generalization is per-statement, so the same `rm ${TARGET}` shape
-- appearing in ten different asks, and possibly twice in one multi-line
-- ask, still stores its statement body exactly once.
CREATE TABLE IF NOT EXISTS approval_statements (
    statement_digest TEXT    NOT NULL PRIMARY KEY,
    rendered          TEXT    NOT NULL,
    -- kaish's open vocabulary ("command", "pipeline", "for", "and_chain",
    -- …) — never CHECK-constrained here because kaish, not this crate,
    -- owns that list and adds to it.
    statement_kind    TEXT    NOT NULL,
    -- Cached at first insert from this statement's own
    -- `approval_statement_vars` rows: 1 iff it has a `binding = 'free'`
    -- variable. This is guarantee 3's fast path — the
    -- `approval_rules_reject_free_variable_allow_rules` trigger below
    -- reads this one column instead of re-scanning
    -- `approval_statement_vars` on every allow-rule insert. Never
    -- recomputed after insert: the statement is immutable, so the answer
    -- can't change under it.
    has_free_vars     INTEGER NOT NULL CHECK (has_free_vars IN (0, 1)),
    created_at        INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- Every command a statement would run — control-structure bodies, `if`
-- conditions, and `$(...)` substitutions included, because kaish's own
-- `plan_program` doc comment is explicit that each of those is a command
-- the statement runs. `backgrounded` is INTEGER 0/1 (SQLite has no BOOLEAN
-- affinity); matches kaish's `PlannedCommand::background`. Keyed on
-- `statement_digest` alone — no `stmt_seq`, since a statement's identity
-- IS its digest now (see file header).
CREATE TABLE IF NOT EXISTS approval_statement_commands (
    statement_digest TEXT    NOT NULL REFERENCES approval_statements(statement_digest),
    cmd_seq          INTEGER NOT NULL,
    name             TEXT    NOT NULL,
    backgrounded     INTEGER NOT NULL CHECK (backgrounded IN (0, 1)),
    PRIMARY KEY (statement_digest, cmd_seq)
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
CREATE TABLE IF NOT EXISTS approval_statement_args (
    statement_digest TEXT    NOT NULL,
    cmd_seq          INTEGER NOT NULL,
    arg_seq          INTEGER NOT NULL,
    value_kind       TEXT    NOT NULL CHECK (value_kind IN ('plain', 'redacted')),
    value_text       TEXT,
    redact_kind      TEXT,
    fingerprint      TEXT,
    PRIMARY KEY (statement_digest, cmd_seq, arg_seq),
    FOREIGN KEY (statement_digest, cmd_seq)
        REFERENCES approval_statement_commands(statement_digest, cmd_seq),
    CHECK (
        (value_kind = 'plain'    AND value_text IS NOT NULL AND redact_kind IS NULL)
        OR
        (value_kind = 'redacted' AND value_text IS NULL     AND redact_kind IS NOT NULL)
    )
);

-- A command's redirects (`>`, `>>`, `2>`, `<`, …). `op` is the operator as
-- kaish rendered it — a merge redirect (`2>&1`) has no target, so `op`
-- alone is `2>&1` and callers store a `plain` empty-string target for it
-- rather than invent a third value_kind, since kaish's own
-- `PlannedRedirect` always carries a `PlannedValue` target (never
-- `Option`). Target reuses the SAME plain/redacted vocabulary as args
-- (see file header: `PlannedRedirect::target` is literally a
-- `PlannedValue`, not a bespoke type) rather than a `target_kind`/
-- `target_text` pairing that would collide in name with "redirect
-- operator kind". Keyed on `statement_digest` alone, same as commands/args.
CREATE TABLE IF NOT EXISTS approval_statement_redirects (
    statement_digest TEXT    NOT NULL,
    cmd_seq          INTEGER NOT NULL,
    redir_seq        INTEGER NOT NULL,
    op               TEXT    NOT NULL,
    value_kind       TEXT    NOT NULL CHECK (value_kind IN ('plain', 'redacted')),
    value_text       TEXT,
    redact_kind      TEXT,
    fingerprint      TEXT,
    PRIMARY KEY (statement_digest, cmd_seq, redir_seq),
    FOREIGN KEY (statement_digest, cmd_seq)
        REFERENCES approval_statement_commands(statement_digest, cmd_seq),
    CHECK (
        (value_kind = 'plain'    AND value_text IS NOT NULL AND redact_kind IS NULL)
        OR
        (value_kind = 'redacted' AND value_text IS NULL     AND redact_kind IS NOT NULL)
    )
);

-- The statement-level variable analysis (`Plan::free_variables` /
-- `bound_variables` in kaish-types). THIS is the table guarantee 3 reads:
-- a row here with `binding = 'free'` means this statement must never back
-- a standing allow-always rule (`rm "$TARGET"` — the statement is
-- pre-resolution by design, so the digest can never be made safe by
-- resolving it later — see the trigger below for the allow/deny
-- asymmetry). `approval_statements.has_free_vars` is the cached
-- OR-reduction of this table, computed once when the statement is first
-- inserted. No `stmt_seq` — a variable belongs to the statement, not to
-- any one ask's use of it.
CREATE TABLE IF NOT EXISTS approval_statement_vars (
    statement_digest TEXT NOT NULL,
    name             TEXT NOT NULL,
    binding          TEXT NOT NULL CHECK (binding IN ('free', 'bound')),
    PRIMARY KEY (statement_digest, name),
    FOREIGN KEY (statement_digest) REFERENCES approval_statements(statement_digest)
);

-- ── Approvals (the ask) ───────────────────────────────────────────────
-- One row per ask, inserted and COMMITTED before any human is ever
-- prompted (guarantee 1 — durable before asked) — a crash between this
-- insert and showing the prompt leaves the row `pending`, never lost.
-- Deliberately NO foreign key on `context_id`: an audit row must outlive
-- the context that spawned it (contexts get archived/wiped; this ledger
-- does not — "retained forever with timestamps for later windowing").
-- `rc_run_id` IS a real foreign key because its target (`rc_runs`) shares
-- this crate's own never-pruned retention policy, so there's no
-- equivalent outlive-the-parent hazard. There is deliberately NO
-- `plan_digest` column here any more — an ask's statement set is
-- many-to-many via `approval_ask_statements` below, and caching a
-- derived "ask digest" here would be a second, driftable source of truth
-- for the same fact (see file header). `status` plus its own timestamp/
-- actor columns stay a plain mutable snapshot (not folded into
-- `approval_events`) because there is exactly one of each per ask — no
-- duplication to fight — and the hottest query this ledger serves
-- ("what's pending right now") wants a direct indexed read, not a
-- last-event-wins derivation. `approval_events` below carries the
-- history a single mutable row cannot: every claim/decide/expire/abandon
-- *attempt*, including the ones this table's own immutability trigger
-- rejects.
CREATE TABLE IF NOT EXISTS approvals (
    request_id       TEXT    NOT NULL PRIMARY KEY,
    context_id       BLOB    NOT NULL,
    principal_id     BLOB    NOT NULL,
    origin           TEXT    NOT NULL CHECK (origin IN ('hook', 'shell_gate', 'kj_verb')),
    instance         TEXT,
    tool             TEXT,
    hook_id          TEXT,
    description      TEXT    NOT NULL,
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

-- An ask's ORDERED statement list — the many-to-many join between an
-- ephemeral `approvals` row and the shared, content-addressed
-- `approval_statements`. This is where `stmt_seq` (ask-relative position)
-- lives now; a statement's own tables (`approval_statement_*`) carry none,
-- because the same statement body can sit at position 0 in one ask and
-- position 2 in another. CASCADE on `request_id` is correct here (unlike
-- `approval_statement_*`, which are never cascaded): this join row is
-- wholly OWNED by the ask, not shared — deleting an ask (never expected in
-- practice, but not disallowed) should drop its ordered-list links without
-- touching the statement content other asks may still reference.
CREATE TABLE IF NOT EXISTS approval_ask_statements (
    request_id       TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    stmt_seq         INTEGER NOT NULL,
    statement_digest TEXT    NOT NULL REFERENCES approval_statements(statement_digest),
    PRIMARY KEY (request_id, stmt_seq)
);
CREATE INDEX IF NOT EXISTS idx_approval_ask_statements_digest
    ON approval_ask_statements(statement_digest);

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
-- gate. `stmt_seq`/`cmd_seq` point at THIS ASK's statement position (via
-- `approval_ask_statements`), not at a statement's timeless identity — a
-- signal is tied to the occasion it fired on, not to the statement body,
-- so it stays ask-relative even though the plan tree itself moved to
-- digest-keying. Both nullable: a signal can speak to the whole ask
-- rather than one command within it.
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
-- A rule generalized from one statement of one decided approval
-- (`learned_from`), matched by (`statement_digest`, `authorized_label`)
-- against FUTURE asks' statements. Deliberately its own table, not a
-- wider-scope row in `approvals`: a rule has no options list, no
-- claim/decide lifecycle, no single originating ask (many future asks'
-- statements, across many different `request_id`s, can match the same
-- rule) — folding it into `approvals` would mean every approvals row
-- grows nullable rule-only columns and every rule grows nullable
-- ask-only columns, for two things read by completely different queries
-- (the human-prompt queue vs. the per-statement redemption fast-path).
-- `learned_from` intentionally has NO cascade: the rule is meant to
-- outlive the one ask that spawned it.
--
-- `statement_digest` and `authorized_label` are BOTH `NOT NULL` — tighter
-- than `approvals`, on purpose: a rule keyed on an unknown digest or an
-- unnamed label wouldn't be a guard that can fail closed, it would be a
-- guard that can never fire at all, which is a worse silent failure mode
-- than refusing to create it.
CREATE TABLE IF NOT EXISTS approval_rules (
    rule_id          TEXT    NOT NULL PRIMARY KEY,
    statement_digest TEXT    NOT NULL REFERENCES approval_statements(statement_digest),
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
-- The redemption fast-path: "is there a live rule for this statement's
-- digest+label?" Partial (WHERE revoked_at IS NULL) so a revoked rule
-- never shadows — or gets confused with — a later live one keyed the
-- same way.
CREATE INDEX IF NOT EXISTS idx_approval_rules_active
    ON approval_rules(statement_digest, authorized_label) WHERE revoked_at IS NULL;

-- Guarantee 3's schema-level backstop: refuse an ALLOW-rule INSERT
-- outright when the referenced statement is known to carry a free
-- variable, or when the statement is NOT known at all. Gated on
-- `NEW.allow = 1` on purpose (fixed 2026-08-14, filed as the second
-- open item from the first build): a standing DENY rule for a
-- free-variable statement is strictly safety-increasing — it can only
-- ever make the gate MORE conservative for that shape — so blocking it
-- has no safety argument and was over-reach. Only an allow-always rule
-- can turn "the human never saw what this resolved to" into "nobody gets
-- asked again", which is the actual hazard. The `COALESCE(..., 1)` is
-- deliberate — a missing `approval_statements` row (e.g. `PRAGMA
-- foreign_keys` was left off, so the `REFERENCES` above didn't actually
-- block it) must fail CLOSED exactly like an unrecognized status must
-- (guarantee 2), not fall through as "no free vars found, so allow it".
-- The single Rust write site (`rules::learn_from_approval`) checks this
-- first — same `allow`-only gate — and returns a specific, readable
-- `FreeVariableRule` error naming the offending variable; this trigger is
-- what still stops it if a future write path ever bypasses that function.
CREATE TRIGGER IF NOT EXISTS approval_rules_reject_free_variable_allow_rules
BEFORE INSERT ON approval_rules
FOR EACH ROW WHEN NEW.allow = 1 AND COALESCE(
    (SELECT has_free_vars FROM approval_statements WHERE statement_digest = NEW.statement_digest), 1
) = 1
BEGIN
    SELECT RAISE(ABORT, 'refusing an allow-always rule: statement has a free variable, or its digest is unrecorded (guarantee 3, fail-closed)');
END;

-- ── Ledger generation (a durable, trigger-maintained change counter) ──
-- The kernel broadcasts one fire-and-forget "something in the ledger
-- changed" notification to connected clients (Amy's ruling: information-
-- light events — no ask id, no status, no content, so a client that
-- doesn't care never has to look). `generation` is the entire payload of
-- that broadcast: a number that goes up exactly when something durable
-- changed underneath it, and never any other time.
--
-- One row, `CHECK (id = 1)` enforced — this is a scalar, not a table, and
-- the CHECK is what a normal INSERT/DELETE cannot violate their way
-- around. Deliberately durable in SQLite rather than an in-memory
-- `AtomicU64`: a counter that resets to zero on every kernel restart
-- cannot tell a client "nothing happened while you were away" from "the
-- process came back up" — only a value that survives the restart in the
-- same place the facts it counts live can make that distinction honestly.
--
-- `generation` is `INTEGER` — SQLite's native affinity, i.e. a signed
-- 64-bit integer — not an unsigned type. An unsigned counter would only
-- buy a lossy conversion at the SQLite boundary (SQLite's INTEGER storage
-- class IS i64; there is no unsigned column type to have one instead),
-- and at human approval-answering rates this counter will never approach
-- 2^63. There is no realistic scenario in which the sign bit unsigned
-- would reclaim is ever the constraint that matters.
CREATE TABLE IF NOT EXISTS ledger_generation (
    id         INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL
);
-- Idempotent seed: `migrate` may run more than once (per `DDL`'s own
-- doc comment above) and must never re-zero an already-advancing counter.
INSERT OR IGNORE INTO ledger_generation (id, generation) VALUES (1, 0);

-- The triggers below are the ONLY writers of `generation` during normal
-- operation (the one deliberate exception, `bump_for_restart` in
-- `generation.rs`, is documented there). No Rust write path in this crate
-- calls `UPDATE ledger_generation` itself — the same reasoning
-- `events.rs`'s module doc gives for `seq` having exactly one
-- implementation applies here too: a table this ledger grows later gets
-- its bump for free by adding one more trigger, instead of depending on
-- every future write function remembering to call a bump helper by hand.
--
-- Each trigger fires `AFTER` its mutation, inside the SAME transaction —
-- a SQLite trigger is never a separate implicit transaction of its own.
-- That is the whole point: the bump can only ever land in the database
-- alongside the fact that caused it. A transaction that inserts an
-- approval and then rolls back takes its trigger-fired bump down with
-- it, so nothing observing `generation` can ever see a change number for
-- a fact that was not actually committed (pinned by
-- `generation_does_not_advance_on_a_rolled_back_transaction` in
-- `generation.rs`'s tests).
CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_approval_insert
AFTER INSERT ON approvals
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_approval_update
AFTER UPDATE ON approvals
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_event_insert
AFTER INSERT ON approval_events
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

-- Rules are included on purpose, not only the ask/event tables: a newly
-- learned allow-always rule changes what a FUTURE ask does before that
-- ask ever exists (guarantee 3's whole mechanism) — a client that only
-- watched `approvals`/`approval_events` would miss the one write that
-- silently changes its own next redemption outcome. All three mutation
-- kinds are covered: `revoke` (`rules::revoke`) is an UPDATE today, but a
-- future DELETE path is covered too, on the same "don't rely on every
-- write path remembering" reasoning as the rest of this section.
CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_rule_insert
AFTER INSERT ON approval_rules
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_rule_update
AFTER UPDATE ON approval_rules
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_rule_delete
AFTER DELETE ON approval_rules
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
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
-- content-addressed body. CASCADE is correct here (unlike
-- `approval_statement_*` above): a run's script log is wholly owned by
-- that run, not shared across runs the way a statement can be shared
-- across asks.
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
-- kernel_db.rs and `approval_statements` above): identical script text
-- across many runs stores once. No cascade target — a body must survive
-- even a run row's own (never-expected-in-practice) deletion, since other
-- runs may share it.
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
            "approval_statements",
            "approval_statement_commands",
            "approval_statement_args",
            "approval_statement_redirects",
            "approval_statement_vars",
            "approvals",
            "approval_ask_statements",
            "approval_options",
            "approval_signals",
            "approval_events",
            "approval_rules",
            "ledger_generation",
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

    /// The other filed item, fixed here: a DENY rule for a free-variable
    /// statement must be permitted at the schema level too (Rust-side
    /// coverage lives in `rules::tests`) — a standing deny can only ever
    /// make the gate more conservative for that shape.
    #[test]
    fn trigger_permits_a_deny_rule_for_a_free_variable_statement() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm ${TARGET}', 'command', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_rules (rule_id, statement_digest, authorized_label, scope, allow)
             VALUES ('r1', 'd1', 'rm target', 'always', 0)",
            [],
        )
        .expect("a deny rule for a free-variable statement must be permitted");
    }

    #[test]
    fn trigger_still_refuses_an_allow_rule_for_a_free_variable_statement() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm ${TARGET}', 'command', 1)",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO approval_rules (rule_id, statement_digest, authorized_label, scope, allow)
                 VALUES ('r1', 'd1', 'rm target', 'always', 1)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("guarantee 3"), "expected the trigger's message, got: {err}");
    }
}
