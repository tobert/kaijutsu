//! The kernel_db schema, applied on open.

pub(super) const SCHEMA: &str = r#"
-- ── Kernel Identity (singleton) ─────────────────────────────────
-- One row per database. The `singleton` column + UNIQUE constraint
-- pins the table to exactly one row; INSERT OR IGNORE on (singleton=1)
-- makes first-open idempotent. No other table partitions by kernel —
-- a database is one kernel.
CREATE TABLE IF NOT EXISTS kernel (
    singleton  INTEGER NOT NULL PRIMARY KEY DEFAULT 1
        CHECK (singleton = 1),
    id         BLOB    NOT NULL UNIQUE,
    founder    BLOB    NOT NULL,
    label      TEXT,
    created_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- A paired caller retains delivery until its result and ask link commit.
CREATE TABLE IF NOT EXISTS approval_pair_handoffs (
    request_id TEXT NOT NULL PRIMARY KEY REFERENCES approvals(request_id) ON DELETE CASCADE,
    released INTEGER NOT NULL DEFAULT 0 CHECK (released IN (0, 1)),
    abandoned_reason TEXT
);

-- The model's ToolCall block whose invocation raised an ask, recorded with it.
CREATE TABLE IF NOT EXISTS approval_tool_calls (
    request_id TEXT NOT NULL PRIMARY KEY REFERENCES approvals(request_id) ON DELETE CASCADE,
    block_id TEXT NOT NULL
);

-- The held drift block a drift ask releases or dismisses, recorded with it.
CREATE TABLE IF NOT EXISTS approval_drift_blocks (
    request_id TEXT NOT NULL PRIMARY KEY REFERENCES approvals(request_id) ON DELETE CASCADE,
    block_id TEXT NOT NULL
);

-- Completion delivery is independent of the single-use execution claim.
CREATE TABLE IF NOT EXISTS execution_notifications (
    kind TEXT NOT NULL CHECK (kind IN ('approval', 'shell')),
    source_id TEXT NOT NULL,
    message TEXT,
    block_id TEXT,
    suppressed_reason TEXT,
    resume_allowed INTEGER NOT NULL DEFAULT 1 CHECK (resume_allowed IN (0, 1)),
    PRIMARY KEY (kind, source_id),
    CHECK (block_id IS NULL OR (message IS NOT NULL AND suppressed_reason IS NULL))
);
CREATE INDEX IF NOT EXISTS execution_notifications_pending
    ON execution_notifications(kind, source_id)
    WHERE block_id IS NULL AND suppressed_reason IS NULL;

-- ── Quiesce (singleton; the row's absence means running) ────────
-- A row here means the kernel is quiesced: it accepts writes but starts no
-- turns. Blocks, ledger answers and config edits still land — a write is
-- inert on its own. Absence of the row is the running state, so a fresh
-- database runs and `resume` is a DELETE.
--
-- The flag is durable because the stop has to outlive the process: a kernel
-- that came back running is not an emergency stop. See docs/system-verbs.md.
CREATE TABLE IF NOT EXISTS system_quiesce (
    singleton   INTEGER NOT NULL PRIMARY KEY DEFAULT 1
        CHECK (singleton = 1),
    -- Always supplied by the writer; the default is a floor, not the path.
    since       INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    quiesced_by BLOB    NOT NULL,
    reason      TEXT
);

-- ── Workspaces ──────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS workspaces (
    workspace_id BLOB NOT NULL PRIMARY KEY,
    label        TEXT NOT NULL UNIQUE,
    description  TEXT,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL,
    archived_at  INTEGER
);

CREATE TABLE IF NOT EXISTS workspace_paths (
    workspace_id BLOB NOT NULL REFERENCES workspaces(workspace_id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    read_only    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (workspace_id, path)
);

-- ── Presets ─────────────────────────────────────────────────────
-- A preset recalls fork filters (`preset_args`); it names no model.
CREATE TABLE IF NOT EXISTS presets (
    preset_id    BLOB NOT NULL PRIMARY KEY,
    label        TEXT NOT NULL UNIQUE,
    description  TEXT,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL
);

-- Normalized, verb-scoped preset arguments (the "filter knobs" of a patch —
-- model knobs stay as columns on `presets`). Repeatable args (e.g. several
-- --exclude ranges) are multiple rows; the composite PK dedups identical ones
-- (the selection algebra is order-free, so a repeat is meaningless). Verb-
-- scoped from day one so the concept generalizes without a migration. See
-- docs/fork-filters.md ("Presets = patch recall").
CREATE TABLE IF NOT EXISTS preset_args (
    preset_id  BLOB NOT NULL REFERENCES presets(preset_id) ON DELETE CASCADE,
    verb       TEXT NOT NULL,
    arg_name   TEXT NOT NULL,
    arg_value  TEXT NOT NULL,
    PRIMARY KEY (preset_id, verb, arg_name, arg_value)
);
CREATE INDEX IF NOT EXISTS idx_preset_args_lookup
    ON preset_args(preset_id, verb);

-- ── Documents (content layer) ──────────────────────────────────
CREATE TABLE IF NOT EXISTS documents (
    document_id  BLOB NOT NULL PRIMARY KEY,
    workspace_id BLOB NOT NULL REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    doc_kind     TEXT NOT NULL DEFAULT 'conversation',
    language     TEXT,
    path         TEXT,
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_documents_workspace
    ON documents(workspace_id);
CREATE INDEX IF NOT EXISTS idx_documents_kind
    ON documents(doc_kind);
CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_path
    ON documents(workspace_id, path) WHERE path IS NOT NULL;

-- ── Contexts (conversation metadata, extends documents) ────────
CREATE TABLE IF NOT EXISTS contexts (
    context_id   BLOB NOT NULL PRIMARY KEY
        REFERENCES documents(document_id) ON DELETE CASCADE,
    label        TEXT,
    system_prompt TEXT,
    context_state TEXT NOT NULL DEFAULT 'live',
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL,
    forked_from  BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    fork_kind    TEXT,
    archived_at  INTEGER,
    concluded_at INTEGER,
    workspace_id BLOB REFERENCES workspaces(workspace_id) ON DELETE SET NULL,
    preset_id    BLOB REFERENCES presets(preset_id) ON DELETE SET NULL,
    last_activity_at INTEGER,
    promoted_at  INTEGER,
    demoted_at   INTEGER,
    paused_at    INTEGER,
    -- Advisory hostname the registering client self-reported at creation
    -- (NULL = unknown). See `ContextRow::origin_host`'s doc comment.
    origin_host  TEXT,
    -- The character performing this context (docs/senpai-kohai.md, "The
    -- model"); NULL for a context that runs no turns (the handoff log, the MCP
    -- hub, a score). A bare id like `created_by`: the kernel never reads
    -- `auth.db`. Triggers refuse a change once set.
    performer    BLOB,
    -- The context this one answers to. NULL only for a character's root
    -- console (`characters.root_ctx`); a trigger refuses any other change to
    -- NULL. `kj context set <ctx> --senpai <ctx>` is the one writer after creation.
    senpai_ctx   BLOB REFERENCES contexts(context_id),
    persona      TEXT
);
-- Labels are unique among LIVE contexts only. An archived context keeps its
-- label as history and leaves this index, so the name can be given again.
CREATE UNIQUE INDEX IF NOT EXISTS idx_contexts_label
    ON contexts(label) WHERE label IS NOT NULL AND archived_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_contexts_workspace
    ON contexts(workspace_id) WHERE workspace_id IS NOT NULL;

-- Contexts whose senpai the migration did not take from the nearest ancestor
-- (`docs/senpai-kohai.md`, "Migration"). `kj context info` prints the note until
-- the senpai is set again.
CREATE TABLE IF NOT EXISTS senpai_migration_report (
    context_id   BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    senpai_ctx   BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    note         TEXT NOT NULL,
    old_reviewer BLOB,
    recorded_at  INTEGER NOT NULL
);

-- ── Context slots (docs/senpai-kohai.md, "The model") ────────────
-- A copy of the character's model (role `model`) or of one character slot
-- (`distill`, `gate`), with every unset parameter filled from the model's
-- `backend_models` row. `max_tokens` is required: every provider request
-- carries it, except on a council server (a backend of kind `mk`). NULL
-- optional parameters are left out of the request. A trigger refuses an
-- UPDATE or DELETE.
CREATE TABLE IF NOT EXISTS context_slots (
    context_id      BLOB    NOT NULL REFERENCES contexts(context_id),
    role            TEXT    NOT NULL,
    backend_id      BLOB    NOT NULL REFERENCES backends(backend_id),
    model           TEXT    NOT NULL CHECK (model != ''),
    max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    context_window  INTEGER CHECK (context_window IS NULL OR context_window > 0),
    temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
    top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
    effort          TEXT,
    thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
    thinking_style  TEXT,
    PRIMARY KEY (context_id, role)
);

-- What `context-characters-v1` did not carry over or could not resolve, and
-- what `context-fold-v1`, `rc-dir-backfill-v1` and `slot-roles-v1` decided,
-- by kind: dropped, unmapped, no_slot, no_gate_slot, no_distill_slot,
-- no_character_slot, senpai_assigned, archived_no_senpai, rc_dir_unset and
-- slot_dropped (no context_id; the principal_id names the character),
-- no_model, console_ambiguous.
CREATE TABLE IF NOT EXISTS context_character_migration_report (
    report_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT    NOT NULL,
    context_id  BLOB    REFERENCES contexts(context_id) ON DELETE CASCADE,
    note        TEXT    NOT NULL,
    recorded_at INTEGER NOT NULL,
    principal_id BLOB   REFERENCES characters(principal_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS context_continuations (
    context_id     BLOB    NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    epoch          INTEGER NOT NULL,
    opened_at      INTEGER NOT NULL,
    last_request_at INTEGER,
    yielded_at     INTEGER,
    signed_off_at  INTEGER,
    invalidated_through INTEGER NOT NULL DEFAULT 0,
    -- `kj interrupt`/RPC interruptContext's own durable fact, distinct from
    -- `signed_off_at` (the performer's own `kj handoff signoff` close).
    -- Refuses automatic resume until the next epoch clears both.
    interrupted_at INTEGER,
    interrupted_by BLOB
);

-- ── Context Edges ───────────────────────────────────────────────
-- Drift edges only. A context's parent is `contexts.forked_from`.
CREATE TABLE IF NOT EXISTS context_edges (
    edge_id    BLOB NOT NULL PRIMARY KEY,
    source_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    target_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    metadata   TEXT,
    created_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);
CREATE INDEX IF NOT EXISTS idx_edges_source ON context_edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON context_edges(target_id);

-- ── Well-Known Context Registry ─────────────────────────────────
-- A fixed set of kernel roles (see `WellKnownRole`), each mapped to at most
-- one context at a time. This is identity, not a label: lost+found is the
-- context that holds the `lost_found` role, never the context named
-- "lost+found" — which is what makes rotation an UPDATE (`set_well_known_
-- context`) instead of a rename. See docs/drifting-dead-letters.md (slice 1).
--
-- ON DELETE CASCADE is deliberate: if the holding context is destroyed the
-- role simply becomes vacant, and the next dead letter mints a fresh holder.
-- That is better than a dangling pointer a reader would have to notice and
-- work around.
CREATE TABLE IF NOT EXISTS well_known_contexts (
    role        TEXT NOT NULL PRIMARY KEY,
    context_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    assigned_at INTEGER NOT NULL
);

-- ── One-Time Migration Markers ──────────────────────────────────
-- A row here means "this named one-time cleanup already ran on this
-- database". It exists for migrations that CANNOT be made naturally
-- idempotent-and-cheap the way `apply_additive_migrations`' guarded ALTERs
-- are — specifically ones whose only way to decide "is there work left?" is
-- an expensive scan. Do NOT reach for this for ordinary schema evolution:
-- an `ALTER TABLE ... ADD COLUMN` guarded by the duplicate-column idiom is
-- still the house style, because it needs no bookkeeping to stay honest.
CREATE TABLE IF NOT EXISTS kernel_migrations (
    name       TEXT    NOT NULL PRIMARY KEY,
    applied_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- ── Op-Log Persistence ──────────────────────────────────────────
-- Append-only journal: each mutation writes one row with the delta.
CREATE TABLE IF NOT EXISTS oplog (
    document_id BLOB    NOT NULL,
    seq         INTEGER NOT NULL,
    payload     BLOB    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (document_id, seq),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
) WITHOUT ROWID;

-- ── Ingest Provenance (docs/ansi-and-beyond.md) ─────────────────
-- The byte-for-byte original an ingest transform consumed to produce a
-- block's content (today: 'ansi-strip'). Kernel-side only — never rides the
-- oplog, snapshots, or the change feed; fetched on demand (`kj block
-- original`) and replayed by `kj block reproject`. Keyed on the BlockId
-- components, NEVER oplog seq: compaction truncates the oplog and would
-- orphan seq-keyed rows, while block ids survive it.
CREATE TABLE IF NOT EXISTS block_provenance (
    context_id    BLOB    NOT NULL,
    principal_id  BLOB    NOT NULL,
    block_seq     INTEGER NOT NULL,
    transform     TEXT    NOT NULL,
    version       INTEGER NOT NULL,
    original      BLOB    NOT NULL,
    created_at    INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (context_id, principal_id, block_seq, transform),
    FOREIGN KEY (context_id) REFERENCES documents(document_id) ON DELETE CASCADE
) WITHOUT ROWID;

-- Compaction checkpoints: latest snapshot per document.
CREATE TABLE IF NOT EXISTS doc_snapshots (
    document_id BLOB    NOT NULL PRIMARY KEY,
    seq         INTEGER NOT NULL,
    version     INTEGER NOT NULL,
    state       BLOB    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
);

-- `input_oplog` and `input_doc_snapshots` (the old input-document tables)
-- are abandoned, not dropped, as of 2026-08-16: the compose scratchpad moved
-- to the draft block (Status::Draft + ephemeral), so the kernel stops
-- reading and writing these two tables here. Neither has a `PRAGMA
-- user_version` gate, so this is not a migration — an existing kernel.db
-- keeps both tables and their rows exactly as they were; a fresh DB never
-- creates them. Do not add DROP TABLE for either; do not touch existing
-- data.

-- ── Context Shell / Env ─────────────────────────────────────────
CREATE TABLE IF NOT EXISTS context_shell (
    context_id  BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    cwd         TEXT,
    updated_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- ── Context Token Usage (token-usage gauge) ──────────────────────
-- A SNAPSHOT of the most recent completed LLM call this context made — NOT
-- a running total across turns. Every provider call resends the entire
-- growing conversation as input, so the last `StreamEvent::Done` observed
-- already reflects the whole history; summing per-call counts across a
-- multi-tool-call turn (or across turns) would multiply-count the same
-- resent history. One row per context, upserted (overwritten) on every
-- `Done` event. `input_tokens` is already normalized across providers: it
-- includes any Claude cache-read/cache-creation tokens (Anthropic reports
-- those separately from `input_tokens` even though they were part of what
-- was sent), so it's directly "total prompt size on the last call" for
-- every provider. `provider`/`model` ride along so a context-window
-- denominator can be attached later by model name — this table carries no
-- window itself (unknown window is represented by its absence upstream,
-- never assumed).
CREATE TABLE IF NOT EXISTS context_usage (
    context_id         BLOB    NOT NULL PRIMARY KEY
        REFERENCES contexts(context_id) ON DELETE CASCADE,
    provider           TEXT    NOT NULL,
    model              TEXT    NOT NULL,
    input_tokens       INTEGER NOT NULL,
    output_tokens      INTEGER NOT NULL,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_ttl_secs     INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL
);

-- ── Turns (per-turn provenance and spend) ────────────────────────
-- One row per model turn, keyed on the `TurnId` the runtime minted. The row
-- is written when the turn starts and finished when it ends; `ended_at` NULL
-- means the turn is running or the kernel stopped before it could say. It
-- names who performed the turn, under which role, and which provider and
-- model answered.
-- The token columns
-- are the provider's RAW counts summed over every call in the turn, so they
-- are spend: Claude's `input_tokens` excludes the cache columns, DeepSeek's
-- already includes `cache_read_tokens`. `context_usage` stays the last-call
-- fill gauge and normalizes `input_tokens`; the two never mix. History is
-- retained: no ON DELETE action on `context_id`, and `performer` is a name as
-- the turn saw it, with no foreign key.
CREATE TABLE IF NOT EXISTS turns (
    turn_id            BLOB    NOT NULL PRIMARY KEY,
    context_id         BLOB    NOT NULL REFERENCES contexts(context_id),
    performer          BLOB    NOT NULL,
    role               TEXT    NOT NULL,
    provider           TEXT    NOT NULL,
    model              TEXT    NOT NULL,
    started_at         INTEGER NOT NULL,
    ended_at           INTEGER,
    stop_reason        TEXT,
    llm_calls          INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_turns_context ON turns(context_id, started_at);

-- ── Model prices (append-only history) ───────────────────────────
-- What a model's tokens cost, keyed by the (provider, model) strings `turns`
-- records. A change inserts a row; the row with the latest `effective_from`
-- at or before a turn's start prices that turn, the later insert winning a
-- tie. Rates are integer micro-dollars per million tokens. A NULL cache rate
-- is unknown, not free. Triggers refuse UPDATE and DELETE so a past turn's
-- cost can always be recomputed.
CREATE TABLE IF NOT EXISTS model_prices (
    price_id              INTEGER PRIMARY KEY AUTOINCREMENT,
    provider              TEXT    NOT NULL CHECK (provider <> ''),
    model                 TEXT    NOT NULL CHECK (model <> ''),
    effective_from        INTEGER NOT NULL,
    input_micros          INTEGER NOT NULL CHECK (input_micros >= 0),
    output_micros         INTEGER NOT NULL CHECK (output_micros >= 0),
    cache_read_micros     INTEGER CHECK (cache_read_micros IS NULL OR cache_read_micros >= 0),
    cache_write_5m_micros INTEGER CHECK (cache_write_5m_micros IS NULL OR cache_write_5m_micros >= 0),
    cache_write_1h_micros INTEGER CHECK (cache_write_1h_micros IS NULL OR cache_write_1h_micros >= 0),
    source                TEXT    NOT NULL DEFAULT '',
    set_by                BLOB,
    created_at            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_model_prices_lookup
    ON model_prices(provider, model, effective_from, price_id);
CREATE TRIGGER IF NOT EXISTS model_prices_no_update BEFORE UPDATE ON model_prices
BEGIN SELECT RAISE(ABORT, 'model_prices is append-only: insert a later price instead'); END;
CREATE TRIGGER IF NOT EXISTS model_prices_no_delete BEFORE DELETE ON model_prices
BEGIN SELECT RAISE(ABORT, 'model_prices is append-only: insert a later price instead'); END;

-- ── Turn tool uses (provider-billed tool calls) ─────────────────
-- Calls a provider ran and billed per call during a turn (a server-side web
-- search), summed per tool. Tokens alone cannot price these. A turn with no
-- row for a tool used it zero times.
CREATE TABLE IF NOT EXISTS turn_tool_uses (
    turn_id BLOB    NOT NULL REFERENCES turns(turn_id),
    tool    TEXT    NOT NULL CHECK (tool <> ''),
    calls   INTEGER NOT NULL CHECK (calls >= 0),
    PRIMARY KEY (turn_id, tool)
);

-- ── Tool prices (append-only history) ────────────────────────────
-- What one provider-billed tool call costs, keyed by the provider string
-- `turns` records and the tool name. Same history rules as `model_prices`:
-- a change inserts a row, the latest `effective_from` at or before a turn's
-- start prices that turn, the later insert wins a tie, and triggers refuse
-- UPDATE and DELETE. The rate is integer micro-dollars per call.
CREATE TABLE IF NOT EXISTS tool_prices (
    price_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    provider        TEXT    NOT NULL CHECK (provider <> ''),
    tool            TEXT    NOT NULL CHECK (tool <> ''),
    effective_from  INTEGER NOT NULL,
    per_call_micros INTEGER NOT NULL CHECK (per_call_micros >= 0),
    source          TEXT    NOT NULL DEFAULT '',
    set_by          BLOB,
    created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tool_prices_lookup
    ON tool_prices(provider, tool, effective_from, price_id);
CREATE TRIGGER IF NOT EXISTS tool_prices_no_update BEFORE UPDATE ON tool_prices
BEGIN SELECT RAISE(ABORT, 'tool_prices is append-only: insert a later price instead'); END;
CREATE TRIGGER IF NOT EXISTS tool_prices_no_delete BEFORE DELETE ON tool_prices
BEGIN SELECT RAISE(ABORT, 'tool_prices is append-only: insert a later price instead'); END;

-- ── Context Tool Bindings (Phase 5, D-54) ───────────────────────
-- Per-context capability allow-set + sticky name resolution, persisted so the
-- loadout survives kernel restart. Deny-by-default: an absent parent row (or an
-- all-empty one) grants nothing. Permissiveness is explicit — the all_instances
-- / all_facades / binding_admin flags below. Normalized per
-- feedback_sql_schema.md: the Rust struct ContextToolBinding reconstructs by
-- joining.
CREATE TABLE IF NOT EXISTS context_bindings (
    context_id    BLOB    NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    all_instances INTEGER NOT NULL DEFAULT 0,  -- "*"        — every broker instance
    all_facades   INTEGER NOT NULL DEFAULT 0,  -- "facade:*" — every facade surface
    binding_admin INTEGER NOT NULL DEFAULT 0,  -- "admin"    — may write any context's loadout
    updated_at    INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- Ordered list of allowed instances (Vec<InstanceId>). order_idx preserves
-- the tiebreak semantic for Auto-mode resolution (D-20 / §4.2).
CREATE TABLE IF NOT EXISTS context_binding_instances (
    context_id  BLOB    NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    instance_id TEXT    NOT NULL,
    order_idx   INTEGER NOT NULL,
    PRIMARY KEY (context_id, instance_id)
);
CREATE INDEX IF NOT EXISTS idx_binding_instances_lookup
    ON context_binding_instances(instance_id);

-- Sticky name resolution map (HashMap<visible_name, (instance, tool)>).
-- visible_name is the LLM-observable name; (instance_id, original_tool) is
-- the resolved target. Preserving this across restart is exit criterion #2.
CREATE TABLE IF NOT EXISTS context_binding_names (
    context_id    BLOB NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    visible_name  TEXT NOT NULL,
    instance_id   TEXT NOT NULL,
    original_tool TEXT NOT NULL,
    PRIMARY KEY (context_id, visible_name)
);
CREATE INDEX IF NOT EXISTS idx_binding_names_target
    ON context_binding_names(context_id, instance_id, original_tool);

-- Tool-granular grants (Vec<(InstanceId, tool)>) — tools allowed even when the
-- whole instance is not bound. order_idx preserves insertion order.
CREATE TABLE IF NOT EXISTS context_binding_tools (
    context_id    BLOB    NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    instance_id   TEXT    NOT NULL,
    original_tool TEXT    NOT NULL,
    order_idx     INTEGER NOT NULL,
    PRIMARY KEY (context_id, instance_id, original_tool)
);

-- Facade grants (Vec<String>) — non-broker tool surfaces (shell,
-- *_input). order_idx preserves insertion order.
CREATE TABLE IF NOT EXISTS context_binding_facades (
    context_id BLOB    NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    facade_id  TEXT    NOT NULL,
    order_idx  INTEGER NOT NULL,
    PRIMARY KEY (context_id, facade_id)
);

-- Authority grants (BTreeSet<String>) — the bare-word `kj` verb caps
-- (drive/fork/drift/transport/operator). A normalized set, not a flag column:
-- a future authority is a new row, no schema migration. Deliberately NOT
-- implied by all_instances, mirroring binding_admin.
CREATE TABLE IF NOT EXISTS context_binding_authorities (
    context_id BLOB NOT NULL REFERENCES context_bindings(context_id) ON DELETE CASCADE,
    authority  TEXT NOT NULL,
    PRIMARY KEY (context_id, authority)
);

CREATE TABLE IF NOT EXISTS context_env (
    context_id BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (context_id, key)
);
CREATE INDEX IF NOT EXISTS idx_ctx_env ON context_env(context_id);

-- Which hosts a context's shell may reach over the network. `docs/egress.md`
-- owns the rule; an empty list reaches nothing.
CREATE TABLE IF NOT EXISTS context_egress (
    context_id BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    host       TEXT NOT NULL,
    PRIMARY KEY (context_id, host)
);
CREATE INDEX IF NOT EXISTS idx_ctx_egress ON context_egress(context_id);

-- ── Hooks (hook persistence follow-up) ──────────────────────────
-- Global match-action hook entries. One row per entry; load ordered by
-- (phase, priority ASC, insertion_idx ASC) so HookTables.entries Vec is
-- reconstructed in insertion order. `insertion_idx` is a monotonic
-- counter per phase computed inside the INSERT statement so callers
-- don't have to track it. `action_kind` is the tagged-union
-- discriminator matching HookActionWire; variant-specific columns are
-- nullable and set only for the matching kind. Not FK-linked — hooks
-- are global, not per-context.
CREATE TABLE IF NOT EXISTS hooks (
    hook_id                  TEXT    NOT NULL PRIMARY KEY,
    phase                    TEXT    NOT NULL,
    priority                 INTEGER NOT NULL,
    insertion_idx            INTEGER NOT NULL,
    match_instance           TEXT,
    match_tool               TEXT,
    match_context            BLOB,
    match_principal          TEXT,
    action_kind              TEXT    NOT NULL,
    action_builtin_name      TEXT,
    -- The kaish body that fires at hook evaluation. ALWAYS set for
    -- `action_kind = 'kaish_invoke'` rows: even script-backed hooks
    -- snapshot the body here at install time
    -- (`feedback_script_snapshot_on_instantiation`).
    action_kaish_body        TEXT,
    -- Provenance: if the hook was installed from a shared script in
    -- `hook_scripts`, this records the originating `script_id`. Read
    -- by admin tooling for traceability; NOT re-resolved at hydrate.
    -- Edits to the source script don't leak into existing hooks.
    action_kaish_script_id   TEXT,
    -- A VFS path read fresh at every fire, never snapshotted
    -- (`action_kind = 'kaish_path'`). Mutually exclusive with
    -- `action_kaish_body` — editing the file at this path reaches the
    -- running hook with no reinstall.
    action_kaish_path        TEXT,
    action_result_text       TEXT,
    action_is_error          INTEGER,
    action_deny_reason       TEXT,
    action_log_target        TEXT,
    action_log_level         TEXT,
    -- `action_kind = 'ask'` (D-57): optional description override for the
    -- permission ask. NULL means "auto-generate `{instance}.{tool}` at
    -- fire time" — see `HookAction::Ask` / `AskSpec`.
    action_ask_description   TEXT,
    updated_at               INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    UNIQUE (phase, insertion_idx)
);
CREATE INDEX IF NOT EXISTS idx_hooks_phase_priority
    ON hooks(phase, priority, insertion_idx);
CREATE INDEX IF NOT EXISTS idx_hooks_kaish_script_id
    ON hooks(action_kaish_script_id);

-- ── Hook scripts (shared kaish bodies) ────────────────────────
-- Reusable kaish bodies for hooks. Hooks reference these by
-- `script_id`; the body is resolved at broker hydrate time. Edits
-- require re-hydration (kernel restart or future reload op) to
-- propagate to live entries. Deletion fails if any hook still
-- references the script.
CREATE TABLE IF NOT EXISTS hook_scripts (
    script_id   TEXT    NOT NULL PRIMARY KEY,
    body        TEXT    NOT NULL,
    description TEXT,
    created_at  INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by  BLOB    NOT NULL,
    updated_at  INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- rc lifecycle scripts are no longer table rows: they live as files under
-- /config/rc (~/.config/kaijutsu/config/rc), seeded to disk at boot. See
-- crate::seed_scripts and rc/mod.rs. A legacy `rc_scripts` table may
-- still exist in pre-files DBs; KernelDb::legacy_rc_scripts migrates it.

-- ── Claude cache breakpoints (per-context policy) ───────────────
-- Populated by rc lifecycle scripts (create/fork/drift) via the
-- kj cache subcommand. Read at LLM stream build time and threaded
-- into BuildOpts.cache_breakpoints. Normalized per
-- feedback_sql_schema.md: one row per breakpoint, ordered by `seq`.
--   target_kind  ∈ {'tools', 'system', 'message_index'}
--   target_index NULL unless target_kind = 'message_index'
--   ttl          ∈ {'ephemeral', 'extended'}
-- The build layer (claude/build.rs) enforces Anthropic's 4-breakpoint
-- cap and dedupes; storage stays liberal so populators don't have to
-- second-guess the wire-layer policy. CASCADE on context delete.
CREATE TABLE IF NOT EXISTS cache_breakpoints (
    context_id   BLOB    NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    target_kind  TEXT    NOT NULL,
    target_index INTEGER,
    ttl          TEXT    NOT NULL,
    created_at   INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (context_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_cache_breakpoints_ctx
    ON cache_breakpoints(context_id);

-- ── Context Hydration Marker (Chameleon batch 2) ────────────────
-- The hydration-marker cost guard: a windowed context hydrates only
-- `[0, marker] ∪ last-window_size` instead of its whole history, so a
-- musician driving at tempo doesn't re-send endless turns every turn.
-- A row exists ONLY for windowed contexts; its ABSENCE = hydrate
-- everything (today's behavior, every non-musician context — the
-- default needs zero rows and touches no read path that lacks a marker).
--   marker       BlockId::to_key() — the pinned-prefix end P; [0,P] always hydrates
--   window_size  the sliding tail length W (last-W blocks)
-- Both-or-nothing (a policy needs both), so one NOT NULL row carries it.
-- The tail slides in memory each turn; this row is upserted only at
-- create + on a durable revision (marker advance). CASCADE on ctx delete.
CREATE TABLE IF NOT EXISTS context_hydration (
    context_id  BLOB    NOT NULL PRIMARY KEY
        REFERENCES contexts(context_id) ON DELETE CASCADE,
    marker      TEXT    NOT NULL,
    window_size INTEGER NOT NULL
);

-- Stage 1 track redesign (docs/tracks.md): `beat_state` is replaced by the
-- per-track `tracks` table + per-(track,context) `attachments` table. The old
-- table is dropped here so dev DBs shed it on the next open; it held only
-- restart-recovery state that is safe to lose on a schema change (cold-start
-- always re-arms stopped, per the original beat_state comment above).
DROP TABLE IF EXISTS beat_state;

-- ── Tracks (clock domains — docs/tracks.md) ─────────────────────
-- One row per named clock domain. The track is PURELY the clock domain: the
-- period (wall-clock driver; a ClockSource trait is Stage 3), the phrase length,
-- the playhead, and the transport switch. The per-context wakeup cadence lives on
-- the attachment (`attachments.wakeup_every`), NOT here — the track says when it
-- beats; each attachment says how often it wants to be woken.
-- `playhead_tick` is NULL until the track first beats; thereafter it is the
-- max committed tick persisted for restart recovery (the live source of truth is
-- the in-memory TrackState.playhead). `playing` mirrors the transport (1=Playing,
-- 0=Stopped); cold-start always re-arms stopped — no surprise token spend.
CREATE TABLE IF NOT EXISTS tracks (
    track_id          TEXT    NOT NULL PRIMARY KEY,
    period_ms         INTEGER NOT NULL,
    beats_per_phrase  INTEGER NOT NULL,
    playhead_tick     INTEGER,           -- NULL until first beat; else max committed tick
    playing           INTEGER NOT NULL DEFAULT 0,  -- 1=Playing, 0=Stopped
    -- The non-producer context whose document holds the track's materialized
    -- score. Set once at creation; never rewritten. NULL on a pre-score row.
    score_context_id  BLOB,
    -- The clock-source discriminator (ClockSourceKind::kind_str). MUTABLE over a
    -- track's life (docs/tracks.md Stage 3): a track sketched on the system clock
    -- can later be slaved to a MIDI master — unlike set-once `score_context_id`,
    -- the clock *source* is circumstantial. `'system'` is the only kind M1 builds.
    clock_kind        TEXT    NOT NULL DEFAULT 'system',
    -- Rename-aside tombstone (`kj transport delete`, docs/tracks.md Transport).
    -- NULL = live. Set together with the `track_id` rename to `{id}~tombstone-
    -- {deleted_at}` in `tombstone_track`'s transaction, so the two always agree.
    -- `~` is outside the track-id charset (`[a-z0-9_-]`, `kaijutsu_types::track`),
    -- enforced on the attach path via `TrackId::new`, so a tombstoned row can
    -- never collide with — or be re-attached under — a live name. The score
    -- context is untouched: it's the durable recovery payload. Recovery is
    -- sqlite-only by decision: `UPDATE tracks SET track_id = '<original>',
    -- deleted_at = NULL WHERE track_id = '<original>~tombstone-<deleted_at>'`.
    deleted_at        INTEGER
);

-- ── Attachments (context → track binding — docs/tracks.md §2+3) ─
-- One row per (track, context) pair. The track stays ignorant of context
-- lifecycles; contexts bind themselves. CASCADE on both sides cleans up
-- automatically when a track is deleted or a context is archived/deleted.
CREATE TABLE IF NOT EXISTS attachments (
    track_id              TEXT    NOT NULL REFERENCES tracks(track_id) ON DELETE CASCADE,
    context_id            BLOB    NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    wakeup_every          INTEGER NOT NULL,        -- beat divisor: wake this context every N beats
    rotate_every_phrases  INTEGER,                 -- NULL = never auto-rotate
    ooda_armed            INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, context_id)
);

-- ── Client Views (docs/shared-state.md "Retiring KV") ───────────
-- One row per app installation: "the context this client last had open,"
-- restored on reconnect. `client_id` is the app's stable per-installation
-- UUID (text, minted client-side — not a kernel-issued id type), so it is
-- TEXT rather than the BLOB convention used for kernel ids. Replaces the
-- KV-era `<client-id>.current_context` string key with a normalized row —
-- strictly less machinery (no envelope, no journal/compaction).
CREATE TABLE IF NOT EXISTS client_views (
    client_id   TEXT    NOT NULL PRIMARY KEY,
    context_id  BLOB    NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- ── LLM backends (SQL-native model config) ─────────────────────
-- Replaces the demolished `models.toml`. SQL is the source of truth: there
-- is no TOML anywhere in the model-config path. The shape is kaibo's
-- "backends + casts".
--
-- `name` is free-form and unique (the handle you type: `anthropic`, `gpt`,
-- `ollama`, `zorak`); `kind` is a CLOSED enum owned by code, mapping onto
-- the `Provider` variants (Claude / DeepSeek / OpenAi / Codex app-server).
-- The old TOML conflated the two — the `[providers.<name>]` table name WAS
-- the type, so you could not run two Anthropic gateways or name a local
-- server anything but `ollama`/`lemonade`/`local`. `codex-app` is the
-- experimental local Codex app-server JSONL backend. `mock` is a test-only
-- kind that `BackendKind::parse` rejects unless the `test-mock` feature is
-- on. `kind` carries no CHECK — SQLite cannot alter one, so widening it for
-- a new provider would only reach a fresh database. `BackendKind::parse`
-- (`llm/config.rs`) is what closes the set instead, called by
-- `upsert_backend` so an unrecognized kind fails at the write that would
-- introduce it, and again at registry build time for a row that arrived by
-- some other route.
--
-- NO api key column, ever: only an env-var NAME or a file PATH. NO `enabled`
-- flag either — a backend you don't want is a row you delete (soft-disable
-- was a second way to say "absent" and it silently ate typo'd kinds at boot).
CREATE TABLE IF NOT EXISTS backends (
    backend_id           BLOB NOT NULL PRIMARY KEY,
    name                 TEXT NOT NULL UNIQUE,
    kind                 TEXT NOT NULL,
    base_url             TEXT,
    api_key_env          TEXT,
    api_key_file         TEXT,
    key_optional         INTEGER NOT NULL DEFAULT 0,
    request_timeout_secs INTEGER CHECK (request_timeout_secs IS NULL OR request_timeout_secs > 0),
    idle_timeout_secs    INTEGER CHECK (idle_timeout_secs IS NULL OR idle_timeout_secs > 0),
    max_concurrent       INTEGER CHECK (max_concurrent IS NULL OR max_concurrent > 0),
    created_at           INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by           BLOB NOT NULL
);

-- Per-model metadata for one backend. `context_window` is an OVERRIDE that
-- is checked first; a model absent here (or present with a NULL window) is
-- an honest "unknown" — for Anthropic it falls through to the live
-- `GET /v1/models/{id}` lookup, everywhere else it stays unknown. Never a
-- guess: a fabricated denominator makes a "% of context used" gauge
-- confidently wrong. `extra` is JSON for sparse provider-specific fields
-- (vision pins, effort-ladder notes) that don't deserve a column yet.
CREATE TABLE IF NOT EXISTS backend_models (
    backend_id     BLOB NOT NULL REFERENCES backends(backend_id) ON DELETE CASCADE,
    model_id       TEXT NOT NULL,
    context_window INTEGER CHECK (context_window IS NULL OR context_window > 0),
    extra          TEXT,
    -- This model's own tunables on this backend. A binding copies them into a
    -- slot whose own column is NULL.
    max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
    top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
    effort          TEXT,
    thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
    thinking_style  TEXT,
    PRIMARY KEY (backend_id, model_id)
);

-- ── Character slots ───────────────────────────────────────────
-- A character's models for its two roles besides its chat model: `distill`
-- (the model that summarizes for forks and pulls) and `gate` (the council
-- server, a backend of kind `mk`, that judges the gate). A NULL tunable takes
-- the model's `backend_models` value when a context is made and copies the
-- slot (docs/senpai-kohai.md, "The model").
CREATE TABLE IF NOT EXISTS character_slots (
    principal_id    BLOB NOT NULL REFERENCES characters(principal_id),
    role            TEXT NOT NULL,
    backend_id      BLOB NOT NULL REFERENCES backends(backend_id),
    model           TEXT NOT NULL CHECK (model != ''),
    max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
    top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
    effort          TEXT,
    thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
    thinking_style  TEXT,
    PRIMARY KEY (principal_id, role)
);
CREATE INDEX IF NOT EXISTS idx_character_slots_backend ON character_slots(backend_id);

-- ── Character models ──────────────────────────────────────────
-- The one model a character runs. A context copies it under role `model`,
-- and under `distill` when the character has no `distill` slot and the model
-- is no council server. A NULL tunable takes the model's `backend_models`
-- value at that copy (docs/senpai-kohai.md, "Where a model comes from").
CREATE TABLE IF NOT EXISTS character_models (
    principal_id    BLOB NOT NULL PRIMARY KEY REFERENCES characters(principal_id),
    backend_id      BLOB NOT NULL REFERENCES backends(backend_id),
    model           TEXT NOT NULL CHECK (model != ''),
    max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
    top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
    effort          TEXT,
    thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
    thinking_style  TEXT
);
CREATE INDEX IF NOT EXISTS idx_character_models_backend ON character_models(backend_id);

-- ── Characters (principal + sheet) ──────────────────────────────
-- The persistent someone a name resolves to: a principal id plus a small,
-- normalized sheet. `principal_id` carries no FK — the kernel never reads
-- `auth.db`, so it is a bare id here exactly as `contexts.created_by` is.
-- See docs/character.md, "Character = principal + sheet".
CREATE TABLE IF NOT EXISTS characters (
    principal_id     BLOB NOT NULL PRIMARY KEY,
    name             TEXT NOT NULL UNIQUE,
    created_at       INTEGER NOT NULL,
    -- Characters retire; they are never deleted. NULL = live.
    retired_at       INTEGER,
    -- The character's handoff-log context, minted lazily by the first `kj
    -- handoff note`/`tail` that needs one (docs/character.md, "The handoff
    -- is an ordinary context"). ON DELETE SET NULL rather than CASCADE: an
    -- archived context is retained work, not a reason to disown the sheet
    -- row that points at it.
    handoff_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    -- A root character is a person: the contexts it performs run no model
    -- turns, and the gate layers decide its kj requests as they decide any
    -- performer's. Accountability itself is a relation between contexts, not
    -- a column here — see docs/character.md, "Roots and rotation".
    root             INTEGER NOT NULL DEFAULT 0,
    -- A root character's root console: the one context of it with no senpai
    -- (docs/senpai-kohai.md, "The model"). A rotation of the root console
    -- moves the pointer in the commit, as it moves `handoff_ctx`.
    root_ctx         BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    -- The directory under /config/rc/ whose lifecycle scripts this
    -- character's contexts run. NULL until set.
    rc_dir           TEXT,
    -- A root character's console and MCP hub: contexts the kernel finds by
    -- pointer. A rotation moves a pointer at its commit, as it moves root_ctx.
    console_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    mcp_hub_ctx      BLOB REFERENCES contexts(context_id) ON DELETE SET NULL
);

-- ── Model aliases ──────────────────────────────────────────────
-- The short `--model` handles (fast/smart/opus/ds-pro/local/…). Replaces
-- the TOML `[model_aliases]` table; same semantics, now with a real FK so a
-- backend can't be deleted out from under an alias.
CREATE TABLE IF NOT EXISTS model_aliases (
    alias      TEXT NOT NULL PRIMARY KEY COLLATE NOCASE,
    backend_id BLOB NOT NULL REFERENCES backends(backend_id),
    model      TEXT NOT NULL CHECK (model != '')
);
CREATE INDEX IF NOT EXISTS idx_model_aliases_backend ON model_aliases(backend_id);

-- ── Embedding service (semantic indexing) ──────────────────────
CREATE TABLE IF NOT EXISTS embedding_config (
    id                INTEGER NOT NULL PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    enabled           INTEGER NOT NULL DEFAULT 1,
    backend           TEXT NOT NULL CHECK (backend != ''),
    model             TEXT NOT NULL CHECK (model != ''),
    dimensions        INTEGER NOT NULL CHECK (dimensions > 0),
    profile           TEXT NOT NULL DEFAULT 'plain',
    timeout_ms        INTEGER NOT NULL CHECK (timeout_ms > 0),
    max_in_flight     INTEGER NOT NULL CHECK (max_in_flight > 0),
    max_context_bytes INTEGER NOT NULL CHECK (max_context_bytes > 0)
);

-- ── Roster (`crates/kaijutsu-kernel/src/roster.rs`) ─────────────────────
-- The live roster: who's around right now, agents and humans alike. A
-- MATERIALIZED VIEW, not a log: current state only, otel traces and logs
-- carry the history. Every presence transition
-- emits an otel event (`kaijutsu-telemetry`) instead of accumulating rows
-- here; these four tables hold only what is true *now*.
--
-- Liveness is deliberately NOT a durable fact carried across a restart —
-- `roster_activity.live` is a computed column, entirely overwritten on every
-- refresh pass (`RosterStore::reconcile`) from the live source of truth
-- (PeerRegistry for `bound`, a recency window over `contexts.last_activity_at`
-- for `recent`). This is what lets the view survive a restart without lying:
-- a `bound` row a prior kernel process wrote is worthless truth the moment
-- the process restarts (no live connections exist yet), and the very next
-- reconciliation prunes it — same "remove, don't downgrade to false" stance
-- as `midi_presence.rs::reap_connection`, chosen for the same reason: nobody
-- observed an absence, so lingering as `live=0` would still be a stale claim
-- about a specific connection that no longer exists. `recent` rows are
-- different in kind: a context's last-activity fact stays meaningful even
-- when stale (it is the honest answer to "when did we last hear from this
-- agent"), so `recent` presence rows are pruned only when the entity itself
-- is gone (archived), never merely for falling outside the liveness window.
--
-- Two clocks on the churny tables, never conflated (`docs/midi.md` "the one
-- timebase"): `observed_at` is the reporting source's own clock — display and
-- provenance only — and `recorded_at` is this kernel's wall clock at write
-- time. ALL recency/staleness comparisons use `recorded_at`, because clients
-- run on three machines against one kernel and a skewed foreign clock must
-- never decide what counts as current.
--
-- Split three ways on churn, not one wide row, so a hot per-refresh write
-- never rewrites the columns that barely change:
-- - `roster_entity` — WHO. Slow-changing (a label rename, basically never).
-- - `roster_presence` — WHERE/HOW it's known live. Slow-ish (host/pid change
--   only on reconnect).
-- - `roster_activity` — the churny liveness/heartbeat columns, rewritten on
--   every refresh tick (~10s) or push event.
-- `roster_status` is self-reported (never kernel-derived) and just as churny
-- as activity, but it answers a different question (what the human SAID,
-- not what the kernel OBSERVED), so it is never folded into `roster_activity`.
--
-- No `source_kernel` column anywhere: kernel federation is explicitly not a
-- goal (Amy: "we aren't doing kernel federation").
CREATE TABLE IF NOT EXISTS roster_entity (
    -- Principal for humans, context for agents — never a third kind in v1.
    entity_kind   TEXT NOT NULL,
    entity_id     BLOB NOT NULL,
    -- Display label only (a principal's nick, a context's label at the time
    -- it was last seen) — never the join key. The join key is always
    -- (entity_kind, entity_id), which downstream tables reference by FK.
    label         TEXT,
    first_seen_at INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (entity_kind, entity_id)
);

-- The joinability guarantee, enforced structurally rather than trusted to
-- every future call site: an `entity_kind = 'context'` row must name a
-- context this kernel actually minted (a row in `contexts`), never a
-- source-asserted id a peer merely claimed. `PeerConfig::principal` is
-- already stamped server-side from the connection (`peers.rs`) — this is
-- the schema-level half of the same rule (`docs/cc-peer.md`: peer
-- attribution is sender-asserted and forgeable). There is no equivalent
-- check for `entity_kind = 'principal'`: this kernel has no `principals`
-- table to check against (a `PrincipalId` is a bare, un-rowed identity
-- here) — that half of the guarantee is enforced entirely by the Rust API
-- (`RosterEntity` is only ever constructed from a `KjCaller`/`PeerConfig`
-- the connection layer already stamped, never parsed from a client-supplied
-- string), documented rather than schema-checked.
CREATE TRIGGER IF NOT EXISTS roster_entity_context_must_exist
BEFORE INSERT ON roster_entity
FOR EACH ROW WHEN NEW.entity_kind = 'context' AND NOT EXISTS (
    SELECT 1 FROM contexts WHERE context_id = NEW.entity_id
)
BEGIN
    SELECT RAISE(ABORT, 'roster_entity: entity_kind=context but entity_id has no contexts row — refusing a source-asserted identity the kernel never minted');
END;

-- One row per live presence, keyed by the producer's own natural key
-- (`source`, `source_local_id`) — e.g. `('peer_registry', '<peer key>')` for
-- a bound app window, `('context_activity', '<context hex>')` for a
-- context's recency presence. `ON DELETE CASCADE` off `roster_entity` means
-- deleting an entity (never expected in v1 — entities are never explicitly
-- deleted, only pruned via presence reconciliation) takes its presence rows
-- with it; deleting a PRESENCE row (reconciliation pruning an absent
-- `bound` peer) cascades to `roster_activity` below, so a pruned presence
-- never leaves a dangling activity row lying about a connection that no
-- longer exists.
CREATE TABLE IF NOT EXISTS roster_presence (
    source          TEXT NOT NULL,
    source_local_id TEXT NOT NULL,
    entity_kind     TEXT NOT NULL,
    entity_id       BLOB NOT NULL,
    -- How liveness is known for this row (design table, three kinds; a
    -- fourth — `heard` — has no v1 producer and is deliberately not built).
    liveness_kind   TEXT NOT NULL,
    host            TEXT,
    pid             INTEGER,
    proc_start      INTEGER,
    first_seen_at   INTEGER NOT NULL,
    PRIMARY KEY (source, source_local_id),
    FOREIGN KEY (entity_kind, entity_id)
        REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_roster_presence_entity
    ON roster_presence(entity_kind, entity_id);

-- The churny half of a presence row: rewritten wholesale on every refresh
-- pass or push event, never the wide identity row above. `live` is the
-- computed-column liveness verdict (see file header) — the ONLY place
-- liveness is stored, and it is meaningless before the boot-refresh has run
-- at least once (`RosterStore::refreshed_at`/boot rule).
CREATE TABLE IF NOT EXISTS roster_activity (
    source          TEXT    NOT NULL,
    source_local_id TEXT    NOT NULL,
    live            INTEGER NOT NULL,
    -- The reporting source's own clock — provenance/display ONLY. Never
    -- compared against `unixepoch()` or another row's `recorded_at` for
    -- staleness (see file header: clock skew across three machines).
    observed_at     INTEGER,
    -- This kernel's wall clock at write time — the ONLY clock recency and
    -- staleness evaluation may use.
    recorded_at     INTEGER NOT NULL,
    PRIMARY KEY (source, source_local_id),
    FOREIGN KEY (source, source_local_id)
        REFERENCES roster_presence(source, source_local_id) ON DELETE CASCADE
);

-- Self-reported status (never kernel-derived): what the human/agent SAID,
-- distinct from what the kernel OBSERVED (`roster_activity`). One row per
-- entity — a status is a property of the entity, not of any one presence/
-- connection, so unlike `roster_presence`/`roster_activity` this is keyed
-- directly on (entity_kind, entity_id). Availability is routing data, never
-- authorization (design record) — nothing in this schema, and nothing that
-- reads this table, may gate an approval on it.
CREATE TABLE IF NOT EXISTS roster_status (
    entity_kind  TEXT NOT NULL,
    entity_id    BLOB NOT NULL,
    status_text  TEXT,
    -- Exactly four states. `locked`/`screensaver` both map to `away` before
    -- they ever reach this column — the client reports the STATE, never the
    -- mechanism, so no platform vocabulary (logind, DBus, CoreGraphics)
    -- reaches the schema.
    availability TEXT NOT NULL,
    observed_at  INTEGER,
    recorded_at  INTEGER NOT NULL,
    PRIMARY KEY (entity_kind, entity_id),
    FOREIGN KEY (entity_kind, entity_id)
        REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
);

-- ── Dirty File Buffers (docs/file-buffers.md) ────────────────────
-- Swap-file marker for the file document cache. A row's PRESENCE is the
-- flag: a path with a row has unsaved edits not yet flushed to disk, and
-- there is no separate boolean to drift out of sync with it. A row is
-- written when a buffer goes dirty and deleted when it flushes (`:w`) or
-- the edit is discarded. On cold start, a path with a row is a swap to
-- announce and preserve; a path with no row is a clean cache, safe to
-- reconcile against disk.
--   context_id         file_context_id(path) — the deterministic UUIDv5,
--                       stored so a reader can locate the document
--                       without recomputing it.
--   loaded_generation   the VFS generation the buffer was loaded at, or
--                       NULL if the attribute read failed at load time.
--                       Compared against the current disk generation at
--                       flush time to refuse a write that would clobber a
--                       disk change made since load.
--   dirtied_at          when this path's current dirty spell began. Not
--                       advanced by a later edit within the same spell —
--                       only a clear followed by a fresh dirty write
--                       resets it.
CREATE TABLE IF NOT EXISTS dirty_file_buffers (
    path              TEXT    NOT NULL PRIMARY KEY,
    context_id        BLOB    NOT NULL,
    loaded_generation INTEGER
        CHECK (loaded_generation IS NULL OR loaded_generation >= 0),
    dirtied_at        INTEGER NOT NULL
        DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

-- ── Context watches (docs/watch.md) ─────────────────────────────
-- One row per thing a context watches. The status columns are the
-- coalesced record since the last delivery: a prompt submit renders every
-- row with `changes > 0` into one notice block and resets them in the same
-- acceptance. A watch never starts a turn.
--   kind              'path' only: a host file behind a VFS path.
--   target            the VFS path as resolved when the watch was added.
--   host_path         the host file the VFS path names; events match on it.
--   changes           bursts of change since the last delivery.
--   first_change_at   the first burst since the last delivery.
--   last_change_at    the latest event since the last delivery.
--   present           whether the file existed at the latest event.
--   delivered_at      the last delivery, NULL before the first.
--   stopped_at        set when promotion brings the context back, so its
--                     watches do not re-arm; `kj watch add` clears it.
CREATE TABLE IF NOT EXISTS context_watches (
    watch_id         INTEGER PRIMARY KEY AUTOINCREMENT,
    context_id       BLOB    NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    kind             TEXT    NOT NULL CHECK (kind IN ('path')),
    target           TEXT    NOT NULL,
    host_path        TEXT    NOT NULL,
    created_by       BLOB    NOT NULL,
    created_at       INTEGER NOT NULL,
    changes          INTEGER NOT NULL DEFAULT 0 CHECK (changes >= 0),
    first_change_at  INTEGER,
    last_change_at   INTEGER,
    present          INTEGER,
    delivered_at     INTEGER,
    stopped_at       INTEGER,
    UNIQUE (context_id, kind, target)
);
CREATE INDEX IF NOT EXISTS idx_context_watches_host_path ON context_watches(host_path);

-- A retired table is dropped, not left behind. Child before parent: this
-- foreign-keys onto `gate_actions`, so it must go first.
DROP TABLE IF EXISTS gate_action_argv;
DROP TABLE IF EXISTS gate_actions;
"#;

