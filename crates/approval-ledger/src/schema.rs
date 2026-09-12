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

use rusqlite::{Connection, OptionalExtension, Result as SqliteResult};

/// The DDL, applied in dependency order (a table's `REFERENCES` target must
/// already exist). Every statement is `IF NOT EXISTS` / `CREATE ... IF NOT
/// EXISTS`, so `migrate` is safe to call every process start against an
/// already-migrated database.
///
/// **`IF NOT EXISTS` reaches a fresh database and no other.** Editing a
/// table's definition here changes what a NEW database gets and nothing
/// about an existing one — a widened `CHECK`, a new column, a changed
/// default all stay invisible where the table already exists. Every such
/// edit needs a step below (`add_rc_runs_script_count_column_if_missing`,
/// `drop_legacy_value_enum_checks`) or it is a change that only works on
/// a machine nobody has been using. There is still no migration-version
/// table; kernel_db's ALTER-TABLE ladder
/// (`kaijutsu-kernel/src/kernel_db.rs`) is the pattern to reach for as
/// more of these accumulate.
///
/// **No column here CHECKs its value against an enumerated set in SQL.**
/// SQLite cannot `ALTER` a `CHECK`, so the moment a set gains one more
/// legal value, the constraint above cannot widen on a database that
/// already ran an earlier `migrate()` — the write that needed the new
/// value fails a constraint the new code cannot see. Every column's legal
/// values are owned instead by the Rust type that writes it (`Origin`,
/// `ApprovalStatus`, `ValueKind`, `VarBinding`, `SignalSourceKind`,
/// `SignalVerdict`, `RuleScope`, `EventKind`, `RcOutcome` — all in
/// `types.rs`), enforced at the one write path per table, where a new
/// variant cannot be forgotten. `drop_legacy_value_enum_checks` below
/// rebuilds every table that still carries one of these from before this
/// rule. A `CHECK` on a fixed, non-enumerated shape is not this and
/// stays: the plain/redacted consistency check on
/// `approval_statement_args`/`approval_statement_redirects`, and the
/// singleton guard on `ledger_generation`.
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
    has_free_vars     INTEGER NOT NULL,
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
    backgrounded     INTEGER NOT NULL,
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
    value_kind       TEXT    NOT NULL,
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
    value_kind       TEXT    NOT NULL,
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
    binding          TEXT NOT NULL,
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
    -- The performer and its assigned reviewer are separate from the
    -- requester/redemption identity. Nullable preserves legacy rows without
    -- inventing provenance for them.
    actor_id         BLOB,
    reviewer_id      BLOB,
    principal_id     BLOB    NOT NULL,
    origin           TEXT    NOT NULL,
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
    status           TEXT    NOT NULL DEFAULT 'pending',
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
    auto_reason      TEXT,
    -- The context's cwd when this ask escalated. An approval authorizes the
    -- operation it was asked about, not a similar one run wherever the
    -- context has since moved to, so the directory has to travel with the
    -- ask rather than with the process that raised it. NULL when the caller
    -- had no persisted cwd to protect.
    cwd              TEXT,
    -- The text to run if this ask is allowed, verbatim. NOT
    -- `authorized_label` and NOT a statement's `rendered`: the first means
    -- different things per origin and the second is a rendering built for a
    -- human to read. NULL means this ask cannot be executed on approval and
    -- its caller must retry instead. docs/gate-shape-b.md.
    exec_source      TEXT,
    -- The separately supplied stdin replayed with exec_source.
    exec_stdin       TEXT,
    -- The block pair this ask's call already authored, which an execution on
    -- approval fills in rather than authoring a second pair beside them.
    -- `BlockId::to_key()` form. NULL when the calling path had no blocks to
    -- name at gate time.
    command_block_id TEXT,
    output_block_id  TEXT,
    -- Who authored the pair above, and therefore who must be told when an
    -- execution on approval fills it: 'turn' (a model turn's own pair, its
    -- turn ended at the gate) or 'session' (a connected session watching its
    -- own blocks, told nothing). NULL when no pair is linked.
    pair_owner       TEXT,
    continuation_epoch INTEGER
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

-- ── Approval environment snapshot ────────────────────────────────────
-- The value every free `${VAR}` this ask's statements read held at ask
-- time, captured once so the human who approves and the execution that
-- later runs see the same values. `value` is NULL when the variable was
-- unset then — a row exists for every free variable name regardless, so
-- an unset variable and no snapshot at all are never confused. Wholly
-- owned by one ask, same as `approval_options` above: CASCADE is safe.
CREATE TABLE IF NOT EXISTS approval_env (
    request_id TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    name       TEXT    NOT NULL,
    value      TEXT,
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
    source_kind TEXT    NOT NULL,
    source_id   TEXT,
    model_id    TEXT,
    weight_hash TEXT,
    stmt_seq    INTEGER,
    cmd_seq     INTEGER,
    label       TEXT,
    score       REAL,
    verdict     TEXT    NOT NULL,
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
--
-- `kind` carries no CHECK — see the growable-value-set rule in this
-- constant's doc comment. The set is `EventKind` in `types.rs`, enforced
-- at the one write path (`events::append`).
CREATE TABLE IF NOT EXISTS approval_events (
    request_id     TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    kind           TEXT    NOT NULL,
    actor          BLOB,
    decided_option TEXT,
    remember_scope TEXT,
    auto_reason    TEXT,
    note           TEXT,
    created_at     INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (request_id, seq)
);

-- ── Refused answer attempts ────────────────────────────────────────────
-- An answer this crate refused on an invariant, which therefore committed
-- nothing else. Today the one reason is `self_approval`: the answering
-- context is the context that raised the ask (`docs/gate-and-shell-split.md`,
-- "No self-approval — the gate's own answer path"). A refusal that is only
-- returned to its caller is invisible to the measurement the gate is tuned
-- on, so it lands here.
--
-- A separate table rather than another `approval_events.kind`: `migrate` is
-- built entirely from `CREATE ... IF NOT EXISTS` and has no ALTER-TABLE path,
-- so widening that column's CHECK would never reach a database that already
-- ran an earlier `migrate()`, and the INSERT would then fail a constraint the
-- new code cannot see. A new table needs no migration machinery at all — the
-- same reasoning `approval_redemptions` below is built on.
--
-- `reason` carries no CHECK — see the growable-value-set rule in `DDL`'s
-- doc comment.
CREATE TABLE IF NOT EXISTS approval_refusals (
    request_id     TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    reason         TEXT    NOT NULL,
    -- Nullable for symmetry with `approval_events.actor`: a refusal can be
    -- recorded for an answerer that named no context, and `actor_context` is
    -- then exactly what was missing.
    actor          BLOB,
    actor_context  BLOB,
    created_at     INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (request_id, seq)
);

-- ── Approval redemptions (single-use consumption of an ALLOWED ask) ────
-- Whether an `allowed` ask has already authorized its one execution
-- (`docs/gate-resume.md`'s single-use redemption: an allowed ask must
-- never become a standing permission — that is what `approval_rules` is
-- for). A separate table, not an `approvals.redeemed_at` column, on
-- purpose: `migrate()` above is built entirely from `CREATE ... IF NOT
-- EXISTS` and has no ALTER-TABLE path (see the file header), so a new
-- column on an existing table would silently never appear in a database
-- that already ran an earlier `migrate()`. A new table needs no migration
-- machinery at all. The PRIMARY KEY is also what makes redemption
-- single-use in the concurrent case: a second `INSERT` for the same
-- `request_id` is decided by SQLite itself (a PRIMARY KEY conflict, or a
-- silent no-op under `INSERT OR IGNORE`), not by a read-then-write the
-- caller has to get right — see `decide::redeem_ask`. CASCADE is correct
-- here (unlike `approval_statement_*`): a redemption row is wholly owned
-- by the one ask it marks spent, never shared.
CREATE TABLE IF NOT EXISTS approval_redemptions (
    request_id  TEXT    NOT NULL PRIMARY KEY REFERENCES approvals(request_id) ON DELETE CASCADE,
    redeemed_at INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
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
    scope            TEXT    NOT NULL,
    allow            INTEGER NOT NULL,
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

-- ── Family rules ──────────────────────────────────────────────────────
-- A human's "always allow kj handoff note": a rule keyed on a command
-- family, never on a statement's text. `family_key` is the gate policy's
-- key space — "kj <verb> [<subcommand>]" in canonical names, or
-- "<command> [<first argument>]" — normalized by the kernel, which owns
-- the verb tables; this crate stores and matches the string.
--
-- Guarantees 3 and 4 do not apply here, and the reason is the key: a
-- family key never reads arguments, so there is no variable value the
-- answerer did not see (guarantee 3) and no authorized text to mismatch
-- (guarantee 4) — generalizing past the arguments is what the answerer
-- asked for. What keeps that safe is structural and lives in the kernel:
-- a family allow is learned only from, and matches only, a command with
-- no redirect, no background flag, no heredoc and plain arguments
-- (`docs/gate-policy-tuning.md`, "Learned family rules").
--
-- No CHECK on `scope`: an enum column's CHECK has to be dropped by a
-- table rebuild when the enum grows (`drop_legacy_value_enum_checks`
-- below), so new enum-shaped columns leave it to Rust.
CREATE TABLE IF NOT EXISTS approval_rule_families (
    rule_id      TEXT    NOT NULL PRIMARY KEY,
    family_key   TEXT    NOT NULL,
    allow        INTEGER NOT NULL CHECK (allow IN (0, 1)),
    scope        TEXT    NOT NULL,
    context_id   BLOB,
    principal_id BLOB,
    created_at   INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB,
    learned_from TEXT    REFERENCES approvals(request_id),
    revoked_at   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_approval_rule_families_active
    ON approval_rule_families(family_key) WHERE revoked_at IS NULL;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_family_insert
AFTER INSERT ON approval_rule_families
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_family_update
AFTER UPDATE ON approval_rule_families
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_family_delete
AFTER DELETE ON approval_rule_families
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

-- An advisory signal attached to an ALREADY-EXISTING ask (`ask::add_signal`
-- — a hook body logging a second scored clause onto the ask its first
-- `--auto-allow` call created) is a durable change on its own: a live
-- `kj ledger show --signals` or an ACP session watching this ask would
-- otherwise see nothing move. A signal inserted alongside its OWN ask's
-- creation (`ask::create_ask`/`create_auto_allowed_ask`) is already covered
-- by the `approvals` insert trigger above — this one exists for the
-- standalone attach, and firing it there too is a harmless no-op re-bump,
-- not a double-count of anything a reader depends on (the invariant is
-- "changed ⇒ bumped", not "bumped exactly once per change").
CREATE TRIGGER IF NOT EXISTS ledger_generation_bump_on_signal_insert
AFTER INSERT ON approval_signals
BEGIN
    UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
END;

-- ── rc run log (the "checklist of what ran") ────────────────────────
-- One row per rc-lifecycle invocation (create/fork/drift/…) for a
-- context. This is what would have made a silently-inert startup rc
-- sweep visible on run one instead of by incident review — a durable,
-- queryable record of which scripts actually fired, not just that the
-- lifecycle verb was called.
-- `script_count` is how many scripts this run intended to execute, set once
-- the run's script list is loaded (before any of them run) — NULL until
-- then, and forever NULL for a run that failed before reaching that point.
-- It is what lets a reader tell a run cancelled part-way (recorded
-- `rc_run_scripts` rows < `script_count`) apart from a run where a script
-- actually failed (rows == `script_count`, one row's `exit_code` nonzero):
-- both leave `outcome = 'failed'` and are otherwise indistinguishable. See
-- `add_rc_runs_script_count_column_if_missing` below — an already-existing
-- database does not get this column from `CREATE TABLE IF NOT EXISTS` alone.
CREATE TABLE IF NOT EXISTS rc_runs (
    run_id       TEXT    NOT NULL PRIMARY KEY,
    context_id   BLOB    NOT NULL,
    context_type TEXT    NOT NULL,
    verb         TEXT    NOT NULL,
    started_at   INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    finished_at  INTEGER,
    outcome      TEXT,
    script_count INTEGER
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
/// call on every process start. Leaves `PRAGMA foreign_keys` (and
/// `PRAGMA legacy_alter_table`) exactly as it found them on return — those
/// are the caller's connection-wide settings to own (see the crate docs)
/// — though `drop_legacy_value_enum_checks` below turns both off/on for
/// the span of its own rebuild transaction and restores the observed
/// values once that transaction ends, success or not; this crate's own
/// invariants (guarantees 2, 3, 6) are enforced by `CHECK`s and triggers
/// that do not depend on FK enforcement being on.
pub fn migrate(conn: &Connection) -> SqliteResult<()> {
    conn.execute_batch(DDL)?;
    // Before the rebuild, not after: a rebuild spec copies a named column
    // list, so any column an old database is missing has to exist before
    // that copy runs or the SELECT names a column that is not there.
    add_approvals_columns_if_missing(conn)?;
    if drop_legacy_value_enum_checks(conn)? {
        // A rebuilt table's indexes and triggers were dropped along with
        // it. A second `DDL` pass puts them back; every other statement is
        // `IF NOT EXISTS` and no-ops. Conditional because a normal start
        // rebuilds nothing and should not pay for a second pass.
        conn.execute_batch(DDL)?;
    }
    add_rc_runs_script_count_column_if_missing(conn)
}

/// Columns `approvals` gained after it shipped. Same shape and the same
/// reason as [`add_rc_runs_script_count_column_if_missing`]: `DDL` gives a
/// fresh database the columns already, and an existing one needs an
/// `ALTER TABLE`, guarded by `PRAGMA table_info` so `migrate` stays safe to
/// call on every process start.
///
/// `ALTER TABLE ... ADD COLUMN` does not rebuild the table, so none of the
/// `ON DELETE CASCADE` hazard that a rebuild carries applies here —
/// `approvals` has six cascading children and a rebuild would fire every one
/// of them under `PRAGMA foreign_keys = ON`.
///
/// This is the crate's second ALTER-TABLE step. A third is the point to
/// build the ladder `kaijutsu-kernel/src/kernel_db.rs` already has rather
/// than adding a fourth one-off function here.
fn add_approvals_columns_if_missing(conn: &Connection) -> SqliteResult<()> {
    let existing: Vec<String> = conn
        .prepare("PRAGMA table_info(approvals)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<SqliteResult<Vec<String>>>()?;
    for (column, ty) in [
        ("actor_id", "BLOB"),
        ("reviewer_id", "BLOB"),
        ("cwd", "TEXT"),
        ("exec_source", "TEXT"),
        ("exec_stdin", "TEXT"),
        ("command_block_id", "TEXT"),
        ("output_block_id", "TEXT"),
        ("pair_owner", "TEXT"),
        ("continuation_epoch", "INTEGER"),
    ] {
        if !existing.iter().any(|name| name == column) {
            conn.execute_batch(&format!("ALTER TABLE approvals ADD COLUMN {column} {ty}"))?;
        }
    }
    Ok(())
}

/// One table `drop_legacy_value_enum_checks` rebuilds to drop a value-enum
/// `CHECK` it shipped before the rule in `DDL`'s doc comment existed.
/// `legacy_check` is the exact `CHECK` clause text unique to the retired
/// shape — never the bare word `CHECK`, since `approval_statement_args`
/// and `approval_statement_redirects` each keep a SECOND `CHECK` (the
/// plain/redacted consistency check) that must survive untouched.
/// `body` is the rebuilt table's full column/key/FK list, exactly what
/// sits between the parens of its `CREATE TABLE`; `columns` is the
/// comma-separated list shared by both shapes, used to copy every row
/// across unchanged.
struct ValueEnumRebuildSpec {
    table: &'static str,
    legacy_check: &'static str,
    body: &'static str,
    columns: &'static str,
}

/// Parents before children, matching the order `DDL` declares them in:
/// `approval_statement_commands`/`_args`/`_redirects`/`_vars` reference
/// `approval_statements`; `approvals` references `rc_runs`;
/// `approval_signals` and `approval_events` reference `approvals`;
/// `approval_rules` references both `approval_statements` and `approvals`.
/// Each spec's create/copy/drop/rename cycle completes before the next one
/// starts, so a child's `REFERENCES` target already exists, under its
/// final name, by the time SQLite resolves it.
const VALUE_ENUM_REBUILD_SPECS: &[ValueEnumRebuildSpec] = &[
    ValueEnumRebuildSpec {
        table: "approval_statements",
        legacy_check: "CHECK (has_free_vars IN (",
        body: "statement_digest TEXT    NOT NULL PRIMARY KEY,
            rendered          TEXT    NOT NULL,
            statement_kind    TEXT    NOT NULL,
            has_free_vars     INTEGER NOT NULL,
            created_at        INTEGER NOT NULL
                DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))",
        columns: "statement_digest, rendered, statement_kind, has_free_vars, created_at",
    },
    ValueEnumRebuildSpec {
        table: "approval_statement_commands",
        legacy_check: "CHECK (backgrounded IN (",
        body: "statement_digest TEXT    NOT NULL REFERENCES approval_statements(statement_digest),
            cmd_seq          INTEGER NOT NULL,
            name             TEXT    NOT NULL,
            backgrounded     INTEGER NOT NULL,
            PRIMARY KEY (statement_digest, cmd_seq)",
        columns: "statement_digest, cmd_seq, name, backgrounded",
    },
    ValueEnumRebuildSpec {
        table: "approval_statement_args",
        legacy_check: "CHECK (value_kind IN (",
        body: "statement_digest TEXT    NOT NULL,
            cmd_seq          INTEGER NOT NULL,
            arg_seq          INTEGER NOT NULL,
            value_kind       TEXT    NOT NULL,
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
            )",
        columns: "statement_digest, cmd_seq, arg_seq, value_kind, value_text, redact_kind, fingerprint",
    },
    ValueEnumRebuildSpec {
        table: "approval_statement_redirects",
        legacy_check: "CHECK (value_kind IN (",
        body: "statement_digest TEXT    NOT NULL,
            cmd_seq          INTEGER NOT NULL,
            redir_seq        INTEGER NOT NULL,
            op               TEXT    NOT NULL,
            value_kind       TEXT    NOT NULL,
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
            )",
        columns: "statement_digest, cmd_seq, redir_seq, op, value_kind, value_text, redact_kind, fingerprint",
    },
    ValueEnumRebuildSpec {
        table: "approval_statement_vars",
        legacy_check: "CHECK (binding IN (",
        body: "statement_digest TEXT NOT NULL,
            name             TEXT NOT NULL,
            binding          TEXT NOT NULL,
            PRIMARY KEY (statement_digest, name),
            FOREIGN KEY (statement_digest) REFERENCES approval_statements(statement_digest)",
        columns: "statement_digest, name, binding",
    },
    ValueEnumRebuildSpec {
        table: "rc_runs",
        legacy_check: "CHECK (outcome IS NULL OR outcome IN (",
        body: "run_id       TEXT    NOT NULL PRIMARY KEY,
            context_id   BLOB    NOT NULL,
            context_type TEXT    NOT NULL,
            verb         TEXT    NOT NULL,
            started_at   INTEGER NOT NULL
                DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
            finished_at  INTEGER,
            outcome      TEXT,
            script_count INTEGER",
        columns: "run_id, context_id, context_type, verb, started_at, finished_at, outcome, script_count",
    },
    ValueEnumRebuildSpec {
        table: "approvals",
        legacy_check: "CHECK (origin IN (",
        body: "request_id       TEXT    NOT NULL PRIMARY KEY,
            context_id       BLOB    NOT NULL,
            actor_id         BLOB,
            reviewer_id      BLOB,
            principal_id     BLOB    NOT NULL,
            origin           TEXT    NOT NULL,
            instance         TEXT,
            tool             TEXT,
            hook_id          TEXT,
            description      TEXT    NOT NULL,
            authorized_label TEXT,
            rc_run_id        TEXT    REFERENCES rc_runs(run_id),
            status           TEXT    NOT NULL DEFAULT 'pending',
            created_at       INTEGER NOT NULL
                DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
            expires_at       INTEGER,
            claimed_at       INTEGER,
            claimed_by       BLOB,
            decided_at       INTEGER,
            decided_by       BLOB,
            decided_option   TEXT,
            remember_scope   TEXT,
            auto_reason      TEXT,
            cwd              TEXT,
            exec_source      TEXT,
            exec_stdin       TEXT,
            command_block_id TEXT,
            output_block_id  TEXT,
            pair_owner       TEXT,
            continuation_epoch INTEGER",
        columns: "request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id, description, \
            authorized_label, rc_run_id, status, created_at, expires_at, claimed_at, claimed_by, \
            decided_at, decided_by, decided_option, remember_scope, auto_reason, cwd, exec_source, exec_stdin, \
            command_block_id, output_block_id, pair_owner, continuation_epoch",
    },
    ValueEnumRebuildSpec {
        table: "approval_signals",
        legacy_check: "CHECK (source_kind IN (",
        body: "request_id  TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
            seq         INTEGER NOT NULL,
            source_kind TEXT    NOT NULL,
            source_id   TEXT,
            model_id    TEXT,
            weight_hash TEXT,
            stmt_seq    INTEGER,
            cmd_seq     INTEGER,
            label       TEXT,
            score       REAL,
            verdict     TEXT    NOT NULL,
            PRIMARY KEY (request_id, seq)",
        columns: "request_id, seq, source_kind, source_id, model_id, weight_hash, stmt_seq, cmd_seq, \
            label, score, verdict",
    },
    ValueEnumRebuildSpec {
        table: "approval_events",
        legacy_check: "CHECK (kind IN (",
        body: "request_id     TEXT    NOT NULL REFERENCES approvals(request_id) ON DELETE CASCADE,
            seq            INTEGER NOT NULL,
            kind           TEXT    NOT NULL,
            actor          BLOB,
            decided_option TEXT,
            remember_scope TEXT,
            auto_reason    TEXT,
            note           TEXT,
            created_at     INTEGER NOT NULL
                DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
            PRIMARY KEY (request_id, seq)",
        columns: "request_id, seq, kind, actor, decided_option, remember_scope, auto_reason, note, created_at",
    },
    ValueEnumRebuildSpec {
        table: "approval_rules",
        legacy_check: "CHECK (scope IN (",
        body: "rule_id          TEXT    NOT NULL PRIMARY KEY,
            statement_digest TEXT    NOT NULL REFERENCES approval_statements(statement_digest),
            authorized_label TEXT    NOT NULL,
            context_id       BLOB,
            principal_id     BLOB,
            scope            TEXT    NOT NULL,
            allow            INTEGER NOT NULL,
            created_at       INTEGER NOT NULL
                DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
            created_by       BLOB,
            learned_from     TEXT    REFERENCES approvals(request_id),
            revoked_at       INTEGER",
        columns: "rule_id, statement_digest, authorized_label, context_id, principal_id, scope, allow, \
            created_at, created_by, learned_from, revoked_at",
    },
];

/// Every table `VALUE_ENUM_REBUILD_SPECS` lists, rebuilt once each to drop
/// the value-enum `CHECK` it shipped with before the rule in `DDL`'s doc
/// comment — see there for why the constraint is gone rather than
/// widened. Guarded per table on that spec's `legacy_check` text inside
/// `sqlite_master.sql`, so an already-rebuilt database, or one that never
/// carried this particular `CHECK`, does nothing; `migrate` must stay safe
/// to call on every process start.
///
/// `approvals` is the `ON DELETE CASCADE` parent of seven tables
/// (`approval_ask_statements`, `approval_options`, `approval_env`,
/// `approval_signals`, `approval_events`, `approval_refusals`,
/// `approval_redemptions`).
/// Rebuilding a table is create, copy, `DROP TABLE`, rename — and with
/// `PRAGMA foreign_keys = ON` (the kernel's connection-wide setting,
/// `kernel_db.rs`), `DROP TABLE` performs an implicit `DELETE FROM`, which
/// fires every one of those cascade edges and deletes the ledger's entire
/// approval history. So the whole set rebuilds inside ONE
/// `foreign_keys = OFF` window: read the caller's current setting, turn it
/// off, run every table's cycle inside a single transaction, run `PRAGMA
/// foreign_key_check` and fail loudly — naming the offending table — if
/// anything was left dangling, commit, then restore the setting that was
/// read, on the error path too, so a caller checking `PRAGMA foreign_keys`
/// afterward sees the same value it had before this ran either way.
/// `PRAGMA foreign_keys` is a no-op inside a transaction, so it is set
/// before `BEGIN` and restored after `COMMIT`/`ROLLBACK`, never inside
/// either.
///
/// `PRAGMA legacy_alter_table` gets the same before/restore treatment,
/// for an unrelated reason: modern SQLite's `ALTER TABLE ... RENAME TO`
/// re-validates every OTHER trigger and view in the schema that mentions
/// the table being renamed, not only the ones on the table itself.
/// `approval_rules_reject_free_variable_allow_rules` (on `approval_rules`)
/// queries `approval_statements` in its body, so renaming
/// `approval_statements` back into place fails that validation with "no
/// such table: main.approval_statements" — the target briefly does not
/// exist between this rebuild's `DROP` and `RENAME` — unless
/// `legacy_alter_table` is ON, which turns that extra validation off.
///
/// Returns whether anything rebuilt, because the caller must then re-run
/// `DDL` once to restore the indexes and triggers a `DROP TABLE` takes
/// down along with its table.
fn drop_legacy_value_enum_checks(conn: &Connection) -> SqliteResult<bool> {
    let mut any_needed = false;
    for spec in VALUE_ENUM_REBUILD_SPECS {
        if spec_still_applies(conn, spec)? {
            any_needed = true;
            break;
        }
    }
    if !any_needed {
        return Ok(false);
    }

    let foreign_keys_were_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    let legacy_alter_table_was_on: i64 = conn.query_row("PRAGMA legacy_alter_table", [], |row| row.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON;")?;
    let rebuilt = rebuild_all_value_enum_tables(conn);
    let restore_sql = format!(
        "PRAGMA foreign_keys = {}; PRAGMA legacy_alter_table = {};",
        if foreign_keys_were_on != 0 { "ON" } else { "OFF" },
        if legacy_alter_table_was_on != 0 { "ON" } else { "OFF" },
    );
    let restored = conn.execute_batch(&restore_sql);

    match (rebuilt, restored) {
        (Ok(()), Ok(())) => Ok(true),
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e),
    }
}

/// Whether `spec.table`'s stored schema still carries its legacy `CHECK`.
/// `false` both when the table has moved past it and when the table does
/// not exist yet — nothing to rebuild either way.
fn spec_still_applies(conn: &Connection, spec: &ValueEnumRebuildSpec) -> SqliteResult<bool> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [spec.table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(sql.is_some_and(|sql| sql.contains(spec.legacy_check)))
}

/// Run every spec's rebuild inside one transaction — a mid-list failure
/// must leave every table on whichever shape it already had, not half
/// rebuilt — then refuse to commit if anything was left dangling.
fn rebuild_all_value_enum_tables(conn: &Connection) -> SqliteResult<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let outcome = (|| -> SqliteResult<()> {
        for spec in VALUE_ENUM_REBUILD_SPECS {
            if spec_still_applies(conn, spec)? {
                rebuild_one_value_enum_table(conn, spec)?;
            }
        }
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let table: String = row.get(0)?;
            // `migrate` returns a plain `rusqlite::Error`, which has no
            // general-purpose "custom message" variant without the `vtab`
            // feature this crate does not enable — `InvalidColumnType` is
            // the same carry-a-message idiom already used elsewhere in
            // this crate (`ask.rs`'s `sql_err`) for the same reason.
            return Err(rusqlite::Error::InvalidColumnType(
                0,
                format!("dropping a value-enum CHECK left a dangling foreign key on {table}; refusing to commit the rebuild"),
                rusqlite::types::Type::Text,
            ));
        }
        Ok(())
    })();

    match &outcome {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(_) => {
            let _ = conn.execute_batch("ROLLBACK;");
        }
    }
    outcome
}

/// Rebuild one table: copy every row into a same-shaped table without
/// `spec.legacy_check`, drop the original, rename the copy into its
/// place. Foreign key enforcement and the surrounding transaction are the
/// caller's responsibility.
fn rebuild_one_value_enum_table(conn: &Connection, spec: &ValueEnumRebuildSpec) -> SqliteResult<()> {
    let staging = format!("{}__check_drop", spec.table);
    conn.execute_batch(&format!(
        "CREATE TABLE {staging} (\n{body}\n);
         INSERT INTO {staging} ({columns})
             SELECT {columns} FROM {table};
         DROP TABLE {table};
         ALTER TABLE {staging} RENAME TO {table};",
        body = spec.body,
        columns = spec.columns,
        table = spec.table,
    ))
}

/// `rc_runs.script_count` was added to `DDL` above after real databases
/// already existed with the old `rc_runs` shape — `CREATE TABLE IF NOT
/// EXISTS` does nothing to a table that is already there, so those
/// databases need an actual `ALTER TABLE` to gain the column. This is the
/// crate's first ALTER-TABLE step (kernel_db's ALTER-TABLE ladder,
/// `kaijutsu-kernel/src/kernel_db.rs`, is the pattern to reach for as more
/// of these accumulate). Guarded by `PRAGMA table_info` so a fresh
/// database — which already has the column from `DDL` — never re-runs the
/// `ALTER TABLE` and fails on a duplicate column; `migrate` must stay safe
/// to call on every process start.
fn add_rc_runs_script_count_column_if_missing(conn: &Connection) -> SqliteResult<()> {
    let has_column = conn
        .prepare("PRAGMA table_info(rc_runs)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<SqliteResult<Vec<String>>>()?
        .iter()
        .any(|name| name == "script_count");
    if !has_column {
        conn.execute_batch("ALTER TABLE rc_runs ADD COLUMN script_count INTEGER")?;
    }
    Ok(())
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

    /// The regression this crate's live database would have hit: a
    /// database that ran `migrate()` before `script_count` existed in
    /// `DDL` must still gain the column on its NEXT `migrate()` call, not
    /// silently stay on the old shape — and running `migrate()` a further
    /// time after that must still be a no-op, not an "duplicate column"
    /// error.
    #[test]
    fn migrate_adds_script_count_to_a_database_created_without_it() {
        let conn = Connection::open_in_memory().unwrap();
        // Hand-build the pre-`script_count` shape rather than calling
        // `migrate()` first — calling it would already create the column,
        // defeating the point of this test.
        conn.execute_batch(
            "CREATE TABLE rc_runs (
                run_id       TEXT    NOT NULL PRIMARY KEY,
                context_id   BLOB    NOT NULL,
                context_type TEXT    NOT NULL,
                verb         TEXT    NOT NULL,
                started_at   INTEGER NOT NULL,
                finished_at  INTEGER,
                outcome      TEXT
            );
            INSERT INTO rc_runs (run_id, context_id, context_type, verb, started_at)
            VALUES ('r1', X'01', 'coder', 'create', 1000);",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let has_column = conn
            .prepare("PRAGMA table_info(rc_runs)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<SqliteResult<Vec<String>>>()
            .unwrap()
            .iter()
            .any(|name| name == "script_count");
        assert!(has_column, "script_count must be added to a pre-existing rc_runs table");

        let script_count: Option<i64> = conn
            .query_row("SELECT script_count FROM rc_runs WHERE run_id = 'r1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(script_count, None, "a pre-existing row gains the column as NULL, not a guessed value");

        // A second migrate() must not try to add the column again.
        migrate(&conn).unwrap();
    }

    /// The same regression as
    /// `migrate_adds_script_count_to_a_database_created_without_it`, for
    /// `pair_owner`: a database that ran `migrate()` before this column
    /// existed must gain it, as NULL, on its next `migrate()`, and the
    /// column must round-trip a written value afterward.
    #[test]
    fn migrate_adds_pair_owner_to_a_database_created_without_it() {
        let conn = Connection::open_in_memory().unwrap();
        // Hand-build the shape `approvals` had once `cwd`/`exec_source`/
        // `command_block_id`/`output_block_id` already existed but
        // `pair_owner` did not, rather than calling `migrate()` first —
        // calling it would already create the column, defeating the point
        // of this test.
        conn.execute_batch(
            "CREATE TABLE approvals (
                request_id       TEXT    NOT NULL PRIMARY KEY,
                context_id       BLOB    NOT NULL,
                principal_id     BLOB    NOT NULL,
                origin           TEXT    NOT NULL,
                instance         TEXT,
                tool             TEXT,
                hook_id          TEXT,
                description      TEXT    NOT NULL,
                authorized_label TEXT,
                rc_run_id        TEXT,
                status           TEXT    NOT NULL DEFAULT 'pending',
                created_at       INTEGER NOT NULL,
                expires_at       INTEGER,
                claimed_at       INTEGER,
                claimed_by       BLOB,
                decided_at       INTEGER,
                decided_by       BLOB,
                decided_option   TEXT,
                remember_scope   TEXT,
                auto_reason      TEXT,
                cwd              TEXT,
                exec_source      TEXT,
                command_block_id TEXT,
                output_block_id  TEXT
            );
            INSERT INTO approvals (request_id, context_id, principal_id, origin, description, created_at)
            VALUES ('r1', X'01', X'02', 'shell_gate', 'x', 1000);",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let columns = conn
            .prepare("PRAGMA table_info(approvals)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<SqliteResult<Vec<String>>>()
            .unwrap();
        assert!(columns.iter().any(|name| name == "pair_owner"));
        assert!(columns.iter().any(|name| name == "actor_id"));
        assert!(columns.iter().any(|name| name == "reviewer_id"));

        let pair_owner: Option<String> = conn
            .query_row("SELECT pair_owner FROM approvals WHERE request_id = 'r1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pair_owner, None, "a pre-existing row gains the column as NULL, not a guessed value");

        let identities: (Option<Vec<u8>>, Option<Vec<u8>>) = conn
            .query_row("SELECT actor_id, reviewer_id FROM approvals WHERE request_id = 'r1'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(identities, (None, None), "legacy rows must not gain guessed actor or reviewer identities");

        conn.execute("UPDATE approvals SET pair_owner = 'turn' WHERE request_id = 'r1'", [])
            .unwrap();
        let round_tripped: String = conn
            .query_row("SELECT pair_owner FROM approvals WHERE request_id = 'r1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(round_tripped, "turn", "the column must round-trip a written value");

        // A second migrate() must not try to add the column again.
        migrate(&conn).unwrap();
    }

    /// Bring a migrated database back to the shape that shipped before
    /// `redeemed` was an event kind, and seed the `approvals` parent row
    /// the rebuilt table's foreign key needs. Migrating first and putting
    /// the old table back is closer to a real deployed database than
    /// hand-building one table on a bare connection: the FK target and the
    /// generation trigger both exist, exactly as they do in kernel.db.
    fn put_approval_events_back_on_its_legacy_shape(conn: &Connection) {
        migrate(conn).unwrap();
        conn.execute_batch(
            "DROP TABLE approval_events;
             CREATE TABLE approval_events (
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
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description)
             VALUES ('r1', X'01', X'02', 'shell_gate', 'x')",
            [],
        )
        .unwrap();
    }

    /// A database carrying the old value-enum `CHECK` on
    /// `approval_events.kind` must lose it on the next `migrate()`, keep
    /// every row it already had, and accept a value the old constraint
    /// rejected. Falsified by reverting `drop_legacy_value_enum_checks` to
    /// `Ok(false)`: the INSERT then failed with "CHECK constraint failed".
    #[test]
    fn migrate_drops_a_legacy_kind_check_and_keeps_the_rows() {
        let conn = Connection::open_in_memory().unwrap();
        put_approval_events_back_on_its_legacy_shape(&conn);
        conn.execute(
            "INSERT INTO approval_events (request_id, seq, kind, note, created_at)
             VALUES ('r1', 0, 'decided', 'kept', 1000)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_events'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the value-enum CHECK must be gone, got: {sql}");

        let (kind, note, created_at): (String, String, i64) = conn
            .query_row(
                "SELECT kind, note, created_at FROM approval_events WHERE request_id = 'r1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), note.as_str(), created_at), ("decided", "kept", 1000),
            "the rebuild must carry every column across, timestamps included");

        conn.execute(
            "INSERT INTO approval_events (request_id, seq, kind) VALUES ('r1', 1, 'redeemed')",
            [],
        )
        .expect("a kind the old CHECK rejected must now insert");

        // Still safe to call on every process start.
        migrate(&conn).unwrap();
    }

    /// The rebuild drops `approval_events`, and SQLite drops that table's
    /// triggers with it. `migrate` runs the rebuild before `DDL` so the
    /// trigger comes back; this pins that ordering. Falsified by moving
    /// `drop_legacy_value_enum_checks` after `execute_batch(DDL)`, which
    /// left the generation counter frozen at its starting value.
    #[test]
    fn the_generation_trigger_survives_the_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        put_approval_events_back_on_its_legacy_shape(&conn);

        migrate(&conn).unwrap();

        let before: i64 = conn
            .query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO approval_events (request_id, seq, kind) VALUES ('r1', 0, 'redeemed')",
            [],
        )
        .unwrap();
        let after: i64 = conn
            .query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert!(after > before, "an event insert must still bump the generation counter");
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
            "approval_env",
            "approval_signals",
            "approval_events",
            "approval_redemptions",
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

    /// `status` (and `origin`) carry no `CHECK` on a fresh database — see
    /// the growable-value-set rule in `DDL`'s doc comment. `ApprovalStatus`
    /// in `types.rs` owns the value set at the one write path instead;
    /// this pins that the schema itself no longer duplicates that job, so
    /// a value `ApprovalStatus` has not caught yet is never silently
    /// rejected by a constraint the Rust type doesn't know about.
    #[test]
    fn status_column_accepts_a_value_no_check_constrains() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description)
             VALUES ('r1', X'01', X'02', 'shell_gate', 'x')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE approvals SET status = 'sideways' WHERE request_id = 'r1'", [])
            .expect("no CHECK constrains status any more");
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

    // ── `drop_legacy_value_enum_checks` coverage ───────────────────────

    /// The pre-rule `CREATE TABLE` text for every table
    /// `VALUE_ENUM_REBUILD_SPECS` rebuilds — what each table looked like
    /// before its value-enum `CHECK` was dropped. Backs both
    /// `put_table_back_on_legacy_shape` and
    /// `rebuilt_table_schema_matches_a_fresh_migration`, which is what
    /// actually pins `DDL` and `VALUE_ENUM_REBUILD_SPECS` together as one
    /// source of truth instead of two that can drift apart.
    const LEGACY_SHAPES: &[(&str, &str)] = &[
        (
            "approval_statements",
            "CREATE TABLE approval_statements (
                statement_digest TEXT    NOT NULL PRIMARY KEY,
                rendered          TEXT    NOT NULL,
                statement_kind    TEXT    NOT NULL,
                has_free_vars     INTEGER NOT NULL CHECK (has_free_vars IN (0, 1)),
                created_at        INTEGER NOT NULL
                    DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
            );",
        ),
        (
            "approval_statement_commands",
            "CREATE TABLE approval_statement_commands (
                statement_digest TEXT    NOT NULL REFERENCES approval_statements(statement_digest),
                cmd_seq          INTEGER NOT NULL,
                name             TEXT    NOT NULL,
                backgrounded     INTEGER NOT NULL CHECK (backgrounded IN (0, 1)),
                PRIMARY KEY (statement_digest, cmd_seq)
            );",
        ),
        (
            "approval_statement_args",
            "CREATE TABLE approval_statement_args (
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
            );",
        ),
        (
            "approval_statement_redirects",
            "CREATE TABLE approval_statement_redirects (
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
            );",
        ),
        (
            "approval_statement_vars",
            "CREATE TABLE approval_statement_vars (
                statement_digest TEXT NOT NULL,
                name             TEXT NOT NULL,
                binding          TEXT NOT NULL CHECK (binding IN ('free', 'bound')),
                PRIMARY KEY (statement_digest, name),
                FOREIGN KEY (statement_digest) REFERENCES approval_statements(statement_digest)
            );",
        ),
        (
            "rc_runs",
            "CREATE TABLE rc_runs (
                run_id       TEXT    NOT NULL PRIMARY KEY,
                context_id   BLOB    NOT NULL,
                context_type TEXT    NOT NULL,
                verb         TEXT    NOT NULL,
                started_at   INTEGER NOT NULL
                    DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                finished_at  INTEGER,
                outcome      TEXT CHECK (outcome IS NULL OR outcome IN ('ok', 'failed', 'abandoned')),
                script_count INTEGER
            );",
        ),
        (
            "approvals",
            "CREATE TABLE approvals (
                request_id       TEXT    NOT NULL PRIMARY KEY,
                context_id       BLOB    NOT NULL,
                principal_id     BLOB    NOT NULL,
                origin           TEXT    NOT NULL CHECK (origin IN ('hook', 'shell_gate', 'kj_verb')),
                instance         TEXT,
                tool             TEXT,
                hook_id          TEXT,
                description      TEXT    NOT NULL,
                authorized_label TEXT,
                rc_run_id        TEXT    REFERENCES rc_runs(run_id),
                status           TEXT    NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'claimed', 'allowed', 'denied', 'expired', 'abandoned')),
                created_at       INTEGER NOT NULL
                    DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                expires_at       INTEGER,
                claimed_at       INTEGER,
                claimed_by       BLOB,
                decided_at       INTEGER,
                decided_by       BLOB,
                decided_option   TEXT,
                remember_scope   TEXT,
                auto_reason      TEXT
            );",
        ),
        (
            "approval_signals",
            "CREATE TABLE approval_signals (
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
            );",
        ),
        (
            "approval_events",
            "CREATE TABLE approval_events (
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
            );",
        ),
        (
            "approval_rules",
            "CREATE TABLE approval_rules (
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
            );",
        ),
    ];

    /// Force `table` back onto its pre-rule `CREATE TABLE` text from
    /// `LEGACY_SHAPES`, on an already-migrated (current-shape) connection.
    /// Dropping one table this way never risks a cascade on its own: FK
    /// enforcement defaults to OFF on a bare `Connection::open_in_memory()`,
    /// and a test that needs it ON (the cascade test below) does the swap
    /// while the table is still empty, before any row is seeded.
    fn put_table_back_on_legacy_shape(conn: &Connection, table: &str) {
        let legacy_sql = LEGACY_SHAPES
            .iter()
            .find(|(name, _)| *name == table)
            .unwrap_or_else(|| panic!("no LEGACY_SHAPES entry for {table}"))
            .1;
        conn.execute_batch(&format!("DROP TABLE {table};\n{legacy_sql}")).unwrap();
    }

    /// Strip what differs between a hand-written `CREATE TABLE` and one
    /// SQLite wrote back after an `ALTER TABLE ... RENAME TO` for reasons
    /// that carry no schema meaning: `--` comments (`DDL`'s column
    /// commentary; a spec's `body` carries none), and the double-quotes
    /// SQLite adds around the renamed identifier
    /// (`CREATE TABLE "approval_statements"`) that `DDL`'s own text never
    /// has. Leaves every column, key, `CHECK`, and `REFERENCES` clause
    /// exactly as significant as it was.
    fn normalize_schema_sql(sql: &str) -> String {
        let mut normalized = String::new();
        for line in sql.lines() {
            let code = line.split("--").next().unwrap_or("").trim();
            if !code.is_empty() {
                normalized.push_str(code);
                normalized.push(' ');
            }
        }
        normalized.replace('"', "")
    }

    /// `VALUE_ENUM_REBUILD_SPECS` and `DDL` are two independent sources of
    /// truth for the same table shapes; this is what catches a future edit
    /// to one that forgets the other. For every rebuilt table, the
    /// (comment- and quoting-normalized) schema text a fresh `migrate()`
    /// produces must equal the schema text produced by rebuilding that
    /// table up from its legacy shape. Falsified by dropping a column from
    /// one spec's `body` without making the matching edit in `DDL`: the
    /// two normalized strings then differ.
    #[test]
    fn rebuilt_table_schema_matches_a_fresh_migration() {
        let fresh = Connection::open_in_memory().unwrap();
        migrate(&fresh).unwrap();

        for spec in VALUE_ENUM_REBUILD_SPECS {
            let fresh_sql: String = fresh
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [spec.table],
                    |row| row.get(0),
                )
                .unwrap();

            let rebuilt_conn = Connection::open_in_memory().unwrap();
            migrate(&rebuilt_conn).unwrap();
            put_table_back_on_legacy_shape(&rebuilt_conn, spec.table);
            migrate(&rebuilt_conn).unwrap();
            let rebuilt_sql: String = rebuilt_conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [spec.table],
                    |row| row.get(0),
                )
                .unwrap();

            assert_eq!(
                normalize_schema_sql(&fresh_sql),
                normalize_schema_sql(&rebuilt_sql),
                "{} rebuilt from its legacy shape must match a fresh migration's shape\nfresh: {fresh_sql}\nrebuilt: {rebuilt_sql}",
                spec.table
            );
        }
    }

    /// The whole point of the `foreign_keys = OFF` window: `approvals` is
    /// the `ON DELETE CASCADE` parent of seven tables, and a naive rebuild
    /// (`DROP TABLE approvals` with FK enforcement left ON) deletes every
    /// one of their rows along with it. This seeds a row in each of the
    /// six, puts `approvals` back on its legacy `CHECK` shape while it is
    /// still empty (so the swap itself cannot cascade anything away),
    /// seeds the parent and child rows, then migrates on a connection with
    /// `foreign_keys = ON` — the kernel's real setting
    /// (`kaijutsu-kernel/src/kernel_db.rs`) — and asserts every child row
    /// survived. Falsified by removing the `foreign_keys = OFF`/restore
    /// bracket from `drop_legacy_value_enum_checks`: every child table
    /// then comes back empty.
    /// A rebuild spec restates a shape `DDL` already holds, so the two can
    /// drift — and a spec that is missing a column does not fail, it
    /// silently rebuilds the table without it and throws that column's data
    /// away.
    ///
    /// This compares a rebuilt table against a freshly-created one, column
    /// for column, which is the only check that catches an omission rather
    /// than a typo. Falsified by deleting `cwd` or `exec_source` from the
    /// `approvals` spec's `body` and `columns`.
    #[test]
    fn a_rebuilt_table_has_the_same_columns_as_a_fresh_one() {
        fn columns_of(conn: &Connection, table: &str) -> Vec<(String, String)> {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let mut cols: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            cols.sort();
            cols
        }

        let fresh = Connection::open_in_memory().unwrap();
        migrate(&fresh).unwrap();

        for spec in VALUE_ENUM_REBUILD_SPECS {
            let rebuilt = Connection::open_in_memory().unwrap();
            migrate(&rebuilt).unwrap();
            // Put the legacy shape back so `migrate`'s rebuild fires on it.
            put_table_back_on_legacy_shape(&rebuilt, spec.table);
            migrate(&rebuilt).unwrap();

            assert_eq!(
                columns_of(&rebuilt, spec.table),
                columns_of(&fresh, spec.table),
                "{}'s rebuild spec has drifted from DDL — a rebuild would drop \
                 the columns it does not name",
                spec.table,
            );
        }
    }

    #[test]
    fn rebuilding_approvals_preserves_every_cascading_child() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&conn).unwrap();

        // `approvals` is empty at this point, so this swap cannot cascade.
        put_table_back_on_legacy_shape(&conn, "approvals");

        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description)
             VALUES ('r1', X'01', X'02', 'shell_gate', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_ask_statements (request_id, stmt_seq, statement_digest)
             VALUES ('r1', 0, 'd1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_options (request_id, seq, option_id, label, kind)
             VALUES ('r1', 0, 'allow', 'Allow', 'allow')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_env (request_id, seq, name, value) VALUES ('r1', 0, 'FOO', 'bar')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_signals (request_id, seq, source_kind, verdict) VALUES ('r1', 0, 'rule', 'allow')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO approval_events (request_id, seq, kind) VALUES ('r1', 0, 'claimed')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO approval_refusals (request_id, seq, reason) VALUES ('r1', 0, 'self_approval')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO approval_redemptions (request_id) VALUES ('r1')", []).unwrap();

        migrate(&conn).unwrap();

        for (table, expected) in [
            ("approval_ask_statements", 1_i64),
            ("approval_options", 1),
            ("approval_env", 1),
            ("approval_signals", 1),
            ("approval_events", 1),
            ("approval_refusals", 1),
            ("approval_redemptions", 1),
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE request_id = 'r1'"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, expected, "{table} lost its row to a cascading DROP of approvals");
        }

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approvals'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the origin/status value-enum CHECKs must be gone, got: {sql}");

        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description, status)
             VALUES ('r2', X'01', X'02', 'a brand new origin', 'x', 'sideways')",
            [],
        )
        .expect("a value the old origin/status CHECKs rejected must now insert");

        let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(foreign_keys, 1, "migrate must restore the caller's foreign_keys setting");

        // `approvals_decided_is_immutable` and the ledger_generation
        // triggers live on `approvals` and are dropped along with it; both
        // must come back via the post-rebuild `DDL` re-run.
        conn.execute("UPDATE approvals SET status = 'denied' WHERE request_id = 'r1'", []).unwrap();
        let err = conn
            .execute("UPDATE approvals SET status = 'allowed' WHERE request_id = 'r1'", [])
            .unwrap_err();
        assert!(err.to_string().contains("guarantee 6"), "the immutability trigger must survive the rebuild");

        let generation: i64 =
            conn.query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert!(generation > 0, "the generation triggers on approvals must survive the rebuild");
    }

    #[test]
    fn migrate_drops_legacy_check_on_approval_statements_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statements");
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 1)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_statements'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the has_free_vars CHECK must be gone, got: {sql}");

        let has_free_vars: i64 = conn
            .query_row(
                "SELECT has_free_vars FROM approval_statements WHERE statement_digest = 'd1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_free_vars, 1, "the rebuild must keep the existing row");

        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d2', 'rm y', 'command', 2)",
            [],
        )
        .expect("a has_free_vars value the old CHECK rejected must now insert");
    }

    #[test]
    fn migrate_drops_legacy_check_on_approval_statement_commands_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 0)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statement_commands");
        conn.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES ('d1', 0, 'rm', 1)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_statement_commands'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the backgrounded CHECK must be gone, got: {sql}");

        let name: String = conn
            .query_row(
                "SELECT name FROM approval_statement_commands WHERE statement_digest = 'd1' AND cmd_seq = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "rm", "the rebuild must keep the existing row");

        conn.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES ('d1', 1, 'ls', 2)",
            [],
        )
        .expect("a backgrounded value the old CHECK rejected must now insert");
    }

    /// A third `value_kind` is not testable the way a plain enum-CHECK
    /// drop is: the surviving plain/redacted consistency `CHECK` matches
    /// on the literal strings `'plain'`/`'redacted'` in both of its
    /// branches, so any other `value_kind` fails it regardless of whether
    /// the retired enum `CHECK` is still present. This test pins the
    /// rebuild and row preservation; the surviving `CHECK`'s own behavior
    /// is pinned separately by
    /// `approval_statement_args_consistency_check_survives_its_own_rebuild`.
    #[test]
    fn migrate_drops_legacy_value_kind_check_on_approval_statement_args_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES ('d1', 0, 'rm', 0)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statement_args");
        conn.execute(
            "INSERT INTO approval_statement_args (statement_digest, cmd_seq, arg_seq, value_kind, value_text)
             VALUES ('d1', 0, 0, 'plain', 'x')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_statement_args'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sql.matches("CHECK").count(),
            1,
            "the value_kind enum CHECK must be gone but the plain/redacted consistency CHECK must survive, got: {sql}"
        );

        let value_text: String = conn
            .query_row(
                "SELECT value_text FROM approval_statement_args
                 WHERE statement_digest = 'd1' AND cmd_seq = 0 AND arg_seq = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value_text, "x", "the rebuild must keep the existing row");
    }

    #[test]
    fn migrate_drops_legacy_value_kind_check_on_approval_statement_redirects_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES ('d1', 0, 'rm', 0)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statement_redirects");
        conn.execute(
            "INSERT INTO approval_statement_redirects
                 (statement_digest, cmd_seq, redir_seq, op, value_kind, value_text)
             VALUES ('d1', 0, 0, '>', 'plain', 'out.txt')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_statement_redirects'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sql.matches("CHECK").count(),
            1,
            "the value_kind enum CHECK must be gone but the plain/redacted consistency CHECK must survive, got: {sql}"
        );

        let op: String = conn
            .query_row(
                "SELECT op FROM approval_statement_redirects
                 WHERE statement_digest = 'd1' AND cmd_seq = 0 AND redir_seq = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, ">", "the rebuild must keep the existing row");
    }

    #[test]
    fn migrate_drops_legacy_check_on_approval_statement_vars_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm ${TARGET}', 'command', 1)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statement_vars");
        conn.execute(
            "INSERT INTO approval_statement_vars (statement_digest, name, binding) VALUES ('d1', 'TARGET', 'free')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_statement_vars'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the binding CHECK must be gone, got: {sql}");

        let binding: String = conn
            .query_row(
                "SELECT binding FROM approval_statement_vars WHERE statement_digest = 'd1' AND name = 'TARGET'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(binding, "free", "the rebuild must keep the existing row");

        conn.execute(
            "INSERT INTO approval_statement_vars (statement_digest, name, binding)
             VALUES ('d1', 'OTHER', 'sideways')",
            [],
        )
        .expect("a binding value the old CHECK rejected must now insert");
    }

    #[test]
    fn migrate_drops_legacy_check_on_rc_runs_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        put_table_back_on_legacy_shape(&conn, "rc_runs");
        conn.execute(
            "INSERT INTO rc_runs (run_id, context_id, context_type, verb, started_at, outcome)
             VALUES ('run1', X'01', 'coder', 'create', 1000, 'ok')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'rc_runs'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!sql.contains("CHECK"), "the outcome CHECK must be gone, got: {sql}");

        let outcome: String =
            conn.query_row("SELECT outcome FROM rc_runs WHERE run_id = 'run1'", [], |r| r.get(0)).unwrap();
        assert_eq!(outcome, "ok", "the rebuild must keep the existing row");

        conn.execute(
            "INSERT INTO rc_runs (run_id, context_id, context_type, verb, started_at, outcome)
             VALUES ('run2', X'01', 'coder', 'create', 2000, 'sideways')",
            [],
        )
        .expect("an outcome value the old CHECK rejected must now insert");

        let has_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_rc_runs_context_started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_index, 1, "idx_rc_runs_context_started must survive the rebuild");
    }

    #[test]
    fn migrate_drops_legacy_checks_on_approval_signals_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description)
             VALUES ('r1', X'01', X'02', 'shell_gate', 'x')",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_signals");
        conn.execute(
            "INSERT INTO approval_signals (request_id, seq, source_kind, verdict) VALUES ('r1', 0, 'rule', 'allow')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_signals'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the source_kind/verdict CHECKs must be gone, got: {sql}");

        let verdict: String = conn
            .query_row("SELECT verdict FROM approval_signals WHERE request_id = 'r1' AND seq = 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(verdict, "allow", "the rebuild must keep the existing row");

        conn.execute(
            "INSERT INTO approval_signals (request_id, seq, source_kind, verdict)
             VALUES ('r1', 1, 'oracle', 'reconsider')",
            [],
        )
        .expect("a source_kind/verdict value the old CHECKs rejected must now insert");

        let generation_before: i64 =
            conn.query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0)).unwrap();
        conn.execute(
            "INSERT INTO approval_signals (request_id, seq, source_kind, verdict) VALUES ('r1', 2, 'rule', 'deny')",
            [],
        )
        .unwrap();
        let generation_after: i64 =
            conn.query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert!(
            generation_after > generation_before,
            "ledger_generation_bump_on_signal_insert must survive the rebuild"
        );
    }

    #[test]
    fn migrate_drops_legacy_checks_on_approval_rules_and_keeps_rows() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm ${TARGET}', 'command', 1)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_rules");
        conn.execute(
            "INSERT INTO approval_rules (rule_id, statement_digest, authorized_label, scope, allow)
             VALUES ('rule1', 'd1', 'rm target', 'always', 0)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'approval_rules'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK"), "the scope/allow CHECKs must be gone, got: {sql}");

        let scope: String =
            conn.query_row("SELECT scope FROM approval_rules WHERE rule_id = 'rule1'", [], |r| r.get(0)).unwrap();
        assert_eq!(scope, "always", "the rebuild must keep the existing row");

        conn.execute(
            "INSERT INTO approval_rules (rule_id, statement_digest, authorized_label, scope, allow)
             VALUES ('rule2', 'd1', 'rm target', 'forever', 0)",
            [],
        )
        .expect("a scope/allow value the old CHECKs rejected must now insert");

        let has_index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_approval_rules_active'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_index, 1, "idx_approval_rules_active must survive the rebuild");

        let err = conn
            .execute(
                "INSERT INTO approval_rules (rule_id, statement_digest, authorized_label, scope, allow)
                 VALUES ('rule3', 'd1', 'rm target', 'always', 1)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("guarantee 3"),
            "approval_rules_reject_free_variable_allow_rules must survive the rebuild"
        );

        let generation_before: i64 =
            conn.query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0)).unwrap();
        conn.execute(
            "INSERT INTO approval_rules (rule_id, statement_digest, authorized_label, scope, allow)
             VALUES ('rule4', 'd1', 'rm target', 'session', 0)",
            [],
        )
        .unwrap();
        let generation_after: i64 =
            conn.query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |r| r.get(0)).unwrap();
        assert!(generation_after > generation_before, "ledger_generation_bump_on_rule_insert must survive the rebuild");
    }

    /// The plain/redacted consistency `CHECK` on `approval_statement_args`
    /// is not a value-enum CHECK and must survive its own table's rebuild
    /// untouched — a `plain` row with no `value_text` is an inconsistent
    /// half-state regardless of whether the retired `value_kind` enum
    /// `CHECK` is still there.
    #[test]
    fn approval_statement_args_consistency_check_survives_its_own_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES ('d1', 0, 'rm', 0)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statement_args");
        migrate(&conn).unwrap();

        let err = conn
            .execute(
                "INSERT INTO approval_statement_args (statement_digest, cmd_seq, arg_seq, value_kind, value_text)
                 VALUES ('d1', 0, 0, 'plain', NULL)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("CHECK"),
            "the plain/redacted consistency CHECK must still reject a half-state row, got: {err}"
        );
    }

    /// A second call must not rebuild a table again once its legacy
    /// `CHECK` is gone — asserted directly on `drop_legacy_value_enum_checks`'s
    /// own return value rather than on the rebuilt schema staying
    /// unchanged (a rebuild is deterministic, so a redundant one would
    /// produce identical text and a text comparison would never catch it).
    /// Targets `approval_statement_args` specifically: it keeps a SECOND,
    /// surviving `CHECK` (the plain/redacted consistency check) after its
    /// own rebuild, which is exactly the case a guard on the bare word
    /// `"CHECK"` — instead of `spec.legacy_check`'s exact text — would get
    /// wrong, re-triggering the rebuild forever.
    #[test]
    fn drop_legacy_value_enum_checks_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
             VALUES ('d1', 'rm x', 'command', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES ('d1', 0, 'rm', 0)",
            [],
        )
        .unwrap();
        put_table_back_on_legacy_shape(&conn, "approval_statement_args");
        conn.execute(
            "INSERT INTO approval_statement_args (statement_digest, cmd_seq, arg_seq, value_kind, value_text)
             VALUES ('d1', 0, 0, 'plain', 'x')",
            [],
        )
        .unwrap();

        let rebuilt_first = drop_legacy_value_enum_checks(&conn).unwrap();
        assert!(rebuilt_first, "the legacy value_kind CHECK must trigger a rebuild the first time");

        let rebuilt_second = drop_legacy_value_enum_checks(&conn).unwrap();
        assert!(
            !rebuilt_second,
            "a second call must not re-rebuild a table that still carries a DIFFERENT, \
             surviving CHECK — this is exactly what a guard on the bare word \"CHECK\" would get wrong"
        );
    }
}
