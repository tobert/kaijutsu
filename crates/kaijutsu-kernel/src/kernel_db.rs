//! SQLite persistence for kernel context metadata, edges, presets, and workspaces.
//!
//! Pattern follows `auth_db.rs` and `db.rs`: single `Connection`, WAL mode,
//! BLOB-encoded typed IDs, in-memory constructor for tests.
//!
//! All timestamps are Unix milliseconds (matching `now_millis()`).
//!
//! ## Known shape, deferred on purpose
//!
//! This file is large (~20 tables, many methods) and the live handle is a single
//! `Arc<Mutex<KernelDb>>`, so every write serializes on one lock. That is a
//! recognized "god-table + single-mutex" smell — and we are **deliberately not
//! splitting it yet**. The pressure that would justify the churn (measured
//! write-contention under concurrent contexts) is not expected any time soon;
//! revisit only when it's an actual, observed problem. See
//! `docs/architecture/kernel.md` for the connection-pool angle. Don't
//! pre-emptively refactor.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Result as SqliteResult, params};
use tracing::{info, warn};

use kaijutsu_types::{
    BackendId, BlockId, CastId, ContextId, ContextState, DocKind, ForkKind,
    KernelId, PresetId, PrincipalId, TurnId, WorkspaceId,
};

use crate::llm::stream::{CacheTarget, CacheTtl};
use crate::mcp::binding::ContextToolBinding;
use crate::mcp::types::InstanceId;
use crate::roster::{Availability, LivenessKind, PresenceSnapshotRow, RosterEntity, RosterRow};

/// SQL for the performer of the `contexts` row aliased `$t`: the principal of
/// its context character, `NULL` for a context with none.
macro_rules! performer_sql {
    ($t:literal) => {
        concat!("(SELECT cc.principal_id FROM context_characters cc WHERE cc.context_id = ", $t, ".context_id)")
    };
}

mod context_characters;
mod model_prices;
#[cfg(test)]
mod test_support;
mod kernel_meta;
mod presets;
mod hooks;
mod client_state;
mod well_known;
mod workspaces;
mod roster;
mod usage;
mod context_bindings;
mod context_env;
mod cache_hydration;
mod tracks;
mod documents;
mod lineage;
mod llm_backends;
mod llm_characters;
mod approvals;
mod review;
mod contexts;
mod context_ring;
#[cfg(test)]
use test_support::*;
pub use model_prices::{CostGroup, ModelPriceRow, NewModelPrice};
pub use context_characters::{ContextCharacterMigrationNote, ContextCharacterRow, ContextSlotRow};
pub use kernel_meta::QuiesceState;
pub use presets::{PresetRow, PresetArg};
pub use hooks::{HookRow, HookScriptRow};
pub use client_state::DirtyFileBuffer;
pub use well_known::WellKnownRole;
pub use workspaces::{WorkspaceRow, WorkspacePathRow};
pub use usage::{ContextUsageRow, TurnStart, RawUsage, TurnRow, TurnTotals, context_used_pct};
pub use context_env::{ContextShellRow, ContextEnvRow};
pub use cache_hydration::DEFAULT_HYDRATION_WINDOW;
pub use tracks::{PersistedTrack, PersistedAttachment};
pub use documents::{DocumentRow, DocSnapshotRow};
pub use lineage::ContextEdgeRow;
pub use llm_backends::{BackendRow, BackendModelRow, ModelTunablesRow, ModelAliasRow, EmbeddingConfigRow};
pub use llm_characters::{CharacterRow, CharacterSlotRow, CharacterModelRow};
pub use approvals::ContinuationOrigin;
pub(crate) use approvals::{HeldAskEnd, record_approval_tool_call, record_approval_drift_block};
pub use review::{ReviewFrom, GovernedAct, Authority, ReviewQuery, EffectiveReviewer};
pub use contexts::ContextRow;
pub use context_ring::{ACTIVE_RING_CAPACITY, DemoteOutcome, PromoteOutcome};

// ============================================================================
// Error type
// ============================================================================

/// Errors from KernelDb operations.
#[derive(Debug, thiserror::Error)]
pub enum KernelDbError {
    /// Underlying SQLite error.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// An `approval-ledger` operation failed. The ledger is DB-injected into
    /// this same connection, so its errors surface during kernel-DB work —
    /// but they are not bare SQLite faults, and flattening them into [`Self::Db`]
    /// would throw away which ledger invariant was actually violated.
    #[error("approval ledger error: {0}")]
    Ledger(#[from] approval_ledger::error::LedgerError),

    /// Entity not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The senpai climb reached a context with no senpai and nobody above it
    /// to land on. Distinct from a fault: an escalation refuses on it, and the
    /// reviewer-context chain ends on it.
    #[error("{0}")]
    NoReviewerAbove(String),

    /// Stored state contradicts an invariant, such as a senpai cycle or a
    /// character with no sheet. A fault to report, never a refusal the caller
    /// can repair by changing its request.
    #[error("{0}")]
    Corrupt(String),

    /// Label already in use for this kernel.
    #[error("label conflict: {0}")]
    LabelConflict(String),

    /// Label contains forbidden characters.
    #[error("invalid label: {0}")]
    InvalidLabel(String),

    /// A `context_env` key is not a legal shell identifier — refused at the
    /// durable write so a bad key is never discovered only when a shell
    /// later tries to materialize it.
    #[error("invalid env key: {0}")]
    InvalidEnvKey(String),

    /// A `context_egress` host is not a bare DNS name, a loopback literal,
    /// or `*` — refused at the durable write, `docs/egress.md`, "The rule".
    #[error("invalid egress host: {0}")]
    InvalidEgressHost(String),

    /// General validation failure.
    #[error("validation error: {0}")]
    Validation(String),

    /// A document with this `document_id` already exists (PRIMARY KEY
    /// conflict on `documents.document_id`) — read back from the DB, never
    /// inferred from message text. See `classify_document_insert_error`.
    #[error("document already exists: {0}")]
    DuplicateDocument(ContextId),
}

pub type KernelDbResult<T> = Result<T, KernelDbError>;

// ============================================================================
// Row types
// ============================================================================

/// The columns of a pre-instance `contexts` row that `senpai-edges-v1` reads.
struct OldContext {
    context_id: ContextId,
    label: Option<String>,
    created_at: i64,
    forked_from: Option<ContextId>,
    archived_at: Option<i64>,
    played_by: Option<PrincipalId>,
}

/// Columns a database written before context characters may lack, added so
/// `senpai-edges-v1` and `context-characters-v1` can read them; the drop that
/// follows `context-characters-v1` removes them (`default_cast_id` is renamed
/// to `cast_id`). Each is `(table, column, definition)`.
const PRE_CONTEXT_CHARACTER_COLUMNS: [(&str, &str, &str); 4] = [
    ("contexts", "cast_id", "BLOB REFERENCES casts(cast_id) ON DELETE SET NULL"),
    ("contexts", "played_by", "BLOB"),
    ("contexts", "senpai_ctx", "BLOB REFERENCES contexts(context_id)"),
    ("characters", "default_cast_id", "BLOB REFERENCES casts(cast_id) ON DELETE SET NULL"),
];

// ============================================================================
// Schema
// ============================================================================

const SCHEMA: &str = r#"
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
    context_type TEXT NOT NULL DEFAULT 'default',
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
    origin_host  TEXT
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

-- ── Context characters (docs/senpai-kohai.md, "The model") ────────
-- One row per context that has a character: the character in this context,
-- the context it answers to, and the persona. A context with no row (scratch,
-- the handoff log, system contexts) runs no turns and routes nothing.
-- `principal_id` is a bare id like `contexts.created_by`: the kernel never
-- reads `auth.db`. `senpai_ctx` is the one column that changes after the
-- context is made (`kj context move`); triggers refuse a change to the rest.
CREATE TABLE IF NOT EXISTS context_characters (
    context_id   BLOB    NOT NULL PRIMARY KEY REFERENCES contexts(context_id),
    principal_id BLOB    NOT NULL,
    senpai_ctx   BLOB    REFERENCES contexts(context_id),
    persona      TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_context_characters_principal
    ON context_characters(principal_id);
CREATE INDEX IF NOT EXISTS idx_context_characters_senpai
    ON context_characters(senpai_ctx) WHERE senpai_ctx IS NOT NULL;

-- A copy of one character slot, with every unset parameter filled from the
-- model's `backend_models` row. `max_tokens` is required: every provider
-- request carries it, except a `gate` or `judge` slot, which names a council
-- server. NULL optional parameters are left out of the request. A trigger
-- refuses an UPDATE or DELETE.
CREATE TABLE IF NOT EXISTS context_slots (
    context_id      BLOB    NOT NULL REFERENCES context_characters(context_id),
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

-- What `context-characters-v1` did not carry over or could not resolve, by
-- kind: dropped, unmapped, no_slot, no_gate_slot, no_distill_slot,
-- no_character_slot.
CREATE TABLE IF NOT EXISTS context_character_migration_report (
    report_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT    NOT NULL,
    context_id  BLOB    REFERENCES contexts(context_id) ON DELETE CASCADE,
    note        TEXT    NOT NULL,
    recorded_at INTEGER NOT NULL
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
-- The model each of a character's roles runs: one slot per role, where a
-- role is a `context_type` (coder, musician, mcp, …) or one of the reserved
-- roles `distill`, `gate` and `judge`. Free-form TEXT with no FK — context
-- types are an open set defined by the rc tree. A NULL tunable takes the
-- model's `backend_models` value when a context character copies the slot
-- (docs/senpai-kohai.md, "The model").
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
-- The one model a character runs wherever no role slot says otherwise. A
-- context character copies it under the context's own type and under
-- `distill`, for each of the two that the character has no slot for. A NULL
-- tunable takes the model's `backend_models` value at that copy
-- (docs/senpai-kohai.md, "Where a model comes from").
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
-- `rc_dir` arrives with the slice that reads it. See docs/character.md, "Character = principal + sheet".
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
    -- A root character is a model context like any other, and its kj
    -- requests always need approval. Accountability itself is a relation
    -- between contexts, not a column here — see docs/character.md, "Roots
    -- and rotation".
    root             INTEGER NOT NULL DEFAULT 0,
    -- A root character's console: the one context of it with no senpai
    -- (docs/senpai-kohai.md, "The model"). A rotation of the console moves the
    -- pointer in the commit, as it moves `handoff_ctx`.
    root_ctx         BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    -- The context type this character usually plays; `kj context create
    -- <character>` takes it when `--type` is absent. NULL = none; a root has none.
    context_type     TEXT
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

-- A retired table is dropped, not left behind. Child before parent: this
-- foreign-keys onto `gate_actions`, so it must go first.
DROP TABLE IF EXISTS gate_action_argv;
DROP TABLE IF EXISTS gate_actions;
"#;

// ============================================================================
// BLOB helpers
// ============================================================================

pub(crate) fn blob_param(id: &[u8; 16]) -> &[u8] {
    id.as_slice()
}

fn read_context_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<ContextId> {
    let bytes: Vec<u8> = row.get(idx)?;
    ContextId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid ContextId bytes".into(),
        )
    })
}

fn read_opt_context_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<ContextId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => ContextId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid ContextId bytes".into(),
            )
        }),
        None => Ok(None),
    }
}

fn read_principal_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<PrincipalId> {
    let bytes: Vec<u8> = row.get(idx)?;
    PrincipalId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid PrincipalId bytes".into(),
        )
    })
}

fn read_opt_principal_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<PrincipalId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => PrincipalId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid PrincipalId bytes".into(),
            )
        }),
        None => Ok(None),
    }
}

fn read_opt_workspace_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<WorkspaceId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => WorkspaceId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid WorkspaceId bytes".into(),
            )
        }),
        None => Ok(None),
    }
}

fn read_opt_preset_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<PresetId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => PresetId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid PresetId bytes".into(),
            )
        }),
        None => Ok(None),
    }
}

fn read_opt_cast_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<CastId>> {
    let bytes: Option<Vec<u8>> = row.get(idx)?;
    match bytes {
        Some(b) => CastId::try_from_slice(&b).map(Some).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Blob,
                "invalid CastId bytes".into(),
            )
        }),
        None => Ok(None),
    }
}

fn read_workspace_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<WorkspaceId> {
    let bytes: Vec<u8> = row.get(idx)?;
    WorkspaceId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid WorkspaceId bytes".into(),
        )
    })
}

fn read_preset_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<PresetId> {
    let bytes: Vec<u8> = row.get(idx)?;
    PresetId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid PresetId bytes".into(),
        )
    })
}

fn read_edge_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<uuid::Uuid> {
    let bytes: Vec<u8> = row.get(idx)?;
    if bytes.len() == 16 {
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes);
        Ok(uuid::Uuid::from_bytes(arr))
    } else {
        Err(rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            "invalid UUID bytes".into(),
        ))
    }
}

/// Parse ContextState from TEXT column.
fn context_state_from_sql(s: &str) -> ContextState {
    ContextState::from_str(s).unwrap_or_else(|_| {
        warn!(state = %s, "unknown ContextState in DB, defaulting to Live");
        ContextState::Live
    })
}

/// Parse ForkKind from a TEXT column. NULL → `None`. An unknown non-null value
/// is a HARD error — a corrupt or forward-incompatible row must crash, never
/// silently degrade to `None` (which would erase fork provenance). Crash over
/// corruption.
fn fork_kind_from_sql(s: Option<String>) -> SqliteResult<Option<ForkKind>> {
    match s {
        None => Ok(None),
        Some(v) => ForkKind::from_str(&v).map(Some).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                format!("unknown ForkKind '{v}'").into(),
            )
        }),
    }
}

/// Parse DocKind from TEXT column.
fn doc_kind_from_sql(s: &str) -> DocKind {
    DocKind::from_str(s).unwrap_or_else(|_| {
        warn!(kind = %s, "unknown DocKind in DB, defaulting to Conversation");
        DocKind::Conversation
    })
}

/// Current time as Unix milliseconds.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Validate a label: no colons allowed (reserved for tag:prefix syntax).
fn validate_label(label: &str) -> KernelDbResult<()> {
    if label.contains(':') {
        return Err(KernelDbError::InvalidLabel(format!(
            "label '{}' must not contain ':'",
            label
        )));
    }
    if label.is_empty() {
        return Err(KernelDbError::InvalidLabel(
            "label must not be empty".to_string(),
        ));
    }
    Ok(())
}

/// Validate a durable export name before storing it. Contextual construction
/// and switching use the same kaish identifier rule: ASCII letter/underscore
/// first, then ASCII alphanumeric/underscore.
fn validate_env_key(key: &str) -> KernelDbResult<()> {
    let mut chars = key.chars();
    let starts_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if !starts_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(KernelDbError::InvalidEnvKey(format!(
            "env key {key:?} is not a valid shell identifier — must start with an ASCII \
             letter or underscore, and contain only ASCII letters, digits, or underscores"
        )));
    }
    Ok(())
}

/// Validate and normalize a `context_egress` host (`docs/egress.md`, "The
/// rule"): a bare DNS name, a loopback literal (`localhost`, `127.0.0.1`,
/// `::1`, or any IP address that parses as loopback), or exactly `*` for
/// every host. No scheme, path, port, whitespace, or wildcard other than
/// the single literal `*`. Lowercases the result, so storage and lookup
/// never disagree on case.
pub(crate) fn validate_egress_host(host: &str) -> KernelDbResult<String> {
    let bad = || {
        KernelDbError::InvalidEgressHost(format!(
            "egress host {host:?} is not valid — use a bare DNS name (example: \"crates.io\"), \
             a loopback literal (\"localhost\", \"127.0.0.1\", \"::1\"), or \"*\" for every host; \
             no scheme, path, port, whitespace, or wildcard other than \"*\""
        ))
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace()) {
        return Err(bad());
    }
    if host == "*" {
        return Ok(host.to_string());
    }
    let lower = host.to_ascii_lowercase();
    // An IP literal is accepted only as a loopback address — any other
    // address is not a form this table understands today.
    if let Ok(ip) = lower.parse::<std::net::IpAddr>() {
        return if ip.is_loopback() { Ok(ip.to_string()) } else { Err(bad()) };
    }
    if lower == "localhost" {
        return Ok(lower);
    }
    // A bare DNS name: no scheme separator, no path, no port, and no
    // wildcard other than the literal `*` already handled above.
    if lower.contains("://") || lower.contains('/') || lower.contains(':') || lower.contains('*') {
        return Err(bad());
    }
    Ok(lower)
}

/// Map constraint violations to typed errors.
///
/// SQLite extended error codes distinguish UNIQUE (2067) from FOREIGN KEY (787).
/// We use `ConstraintViolation` as the primary code for both, so we check the
/// extended code to produce the right error variant.
fn map_unique_violation(e: rusqlite::Error, msg: impl Into<String>) -> KernelDbError {
    if let rusqlite::Error::SqliteFailure(err, ref detail) = e
        && err.code == rusqlite::ErrorCode::ConstraintViolation
    {
        // SQLITE_CONSTRAINT_FOREIGNKEY = 787
        if err.extended_code == 787 {
            let detail_str = detail.as_deref().unwrap_or("foreign key constraint failed");
            return KernelDbError::Validation(detail_str.to_string());
        }
        // SQLITE_CONSTRAINT_UNIQUE = 2067, or any other constraint
        return KernelDbError::LabelConflict(msg.into());
    }
    KernelDbError::Db(e)
}

/// True when `e` is SQLite's "duplicate column name" error — the expected,
/// ignorable outcome of `apply_additive_migrations` re-running an `ALTER
/// TABLE ... ADD COLUMN` against a DB that already has the column (either a
/// fresh DB created from the current `SCHEMA`, or an already-migrated one).
/// SQLite has no dedicated extended code for this (it's a generic
/// `SQLITE_ERROR`, surfaced by rusqlite as `ErrorCode::Unknown`), so we match
/// on the message text — the same shape SQLite itself uses across versions.
fn is_duplicate_column_error(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, Some(msg))
            if err.code == rusqlite::ErrorCode::Unknown
                && msg.starts_with("duplicate column name")
    )
}

// ============================================================================
// KernelDb
// ============================================================================

/// SQLite database for kernel context metadata.
pub struct KernelDb {
    conn: Connection,
    /// Owns the throwaway directory a `temporary()` database was opened
    /// under; `None` for every `open()`. Held only for its `Drop` — removing
    /// the directory (and the `.db` file inside it) once nothing can reach
    /// this handle anymore.
    #[cfg(any(test, feature = "test-util"))]
    _temp_dir: Option<tempfile::TempDir>,
}

impl KernelDb {
    // ========================================================================
    // Quiesce
    // ========================================================================

    pub(crate) fn conn_for_ledger(&self) -> &Connection {
        &self.conn
    }

    /// The connection, mutably, for tests that install a SQLite authorizer
    /// to inject a read or write fault (`Connection::authorizer` takes
    /// `&mut self`).
    #[cfg(test)]
    pub(crate) fn conn_for_test_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Under WAL, `synchronous = NORMAL` fsyncs at checkpoint instead of on
    /// every commit, and stays consistent through a process crash; only a
    /// power loss or kernel panic can drop the most recent commits. The
    /// default `FULL` fsyncs every commit, and a streaming turn commits once
    /// per delta.
    ///
    /// `journal_size_limit` caps how large the `-wal` file is allowed to grow
    /// before a checkpoint truncates it back down; SQLite's default is -1
    /// (no limit), which lets the WAL's high-water mark become permanent disk
    /// cost with no correctness benefit.
    fn init_connection(conn: &Connection) -> SqliteResult<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;
             PRAGMA journal_size_limit = 67108864;",
        )?;
        Ok(())
    }

    /// Column backfills and guarded configuration migrations for existing DBs.
    /// An `embedding_config` in an older shape (a builtin model or a service
    /// endpoint) is replaced by an empty table: the kernel ships no backend
    /// to put in its place.
    /// The project's stance is "schema is truth, bump = wipe", but a single
    /// `ADD COLUMN ... DEFAULT 0` is cheap and spares a live kernel a wipe.
    /// Each ALTER is guarded: a "duplicate column" error on a fresh DB (the
    /// column is already in `SCHEMA`) is expected and ignored. Anything else
    /// — a locked file, a genuinely malformed ALTER — fails loud instead of
    /// leaving the DB silently short a column that later code assumes exists.
    fn apply_additive_migrations(conn: &Connection) -> KernelDbResult<()> {
        let legacy_embedding: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('embedding_config'))
                AND NOT EXISTS(SELECT 1 FROM pragma_table_info('embedding_config') WHERE name = 'backend')",
            [], |row| row.get(0),
        )?;
        if legacy_embedding {
            // The builtin-model and service-endpoint rows name no backend and
            // there is no default to supply, so the rebuilt table starts empty.
            // The warning names what the row held, so the operator can
            // choose a backend and model for it.
            let held_column = ["endpoint", "model_dir"].into_iter().find(|column| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('embedding_config') WHERE name = ?1)",
                    [column], |row| row.get::<_, bool>(0),
                ).unwrap_or(false)
            });
            let held: Option<String> = match held_column {
                Some(column) => conn
                    .query_row(&format!("SELECT {column} FROM embedding_config WHERE id = 1"), [], |row| row.get(0))
                    .optional()?,
                None => None,
            };
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("DROP TABLE embedding_config;
                CREATE TABLE embedding_config (
                    id INTEGER NOT NULL PRIMARY KEY DEFAULT 1 CHECK (id = 1),
                    enabled INTEGER NOT NULL DEFAULT 1,
                    backend TEXT NOT NULL CHECK (backend != ''),
                    model TEXT NOT NULL CHECK (model != ''),
                    dimensions INTEGER NOT NULL CHECK (dimensions > 0),
                    profile TEXT NOT NULL DEFAULT 'plain',
                    timeout_ms INTEGER NOT NULL CHECK (timeout_ms > 0),
                    max_in_flight INTEGER NOT NULL CHECK (max_in_flight > 0),
                    max_context_bytes INTEGER NOT NULL CHECK (max_context_bytes > 0)
                );")?;
            tx.commit()?;
            tracing::warn!(held = held.as_deref().unwrap_or("nothing"), "dropped the old embedding configuration; \
                the semantic index is off until embedding_config names a backend and model (docs/synthesis.md)");
        }
        // A database written before context characters still has the columns
        // they replaced; `contexts.provider` is in every such database and in
        // no other. Decided before anything below changes it.
        let pre_context_character = Self::has_column(conn, "contexts", "provider")?;
        let alters = [
            "ALTER TABLE context_continuations ADD COLUMN invalidated_through INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE approval_pair_handoffs ADD COLUMN abandoned_reason TEXT",
            "ALTER TABLE contexts ADD COLUMN concluded_at INTEGER",
            "ALTER TABLE tracks ADD COLUMN score_context_id BLOB",
            "ALTER TABLE tracks ADD COLUMN clock_kind TEXT NOT NULL DEFAULT 'system'",
            "ALTER TABLE contexts ADD COLUMN last_activity_at INTEGER",
            "ALTER TABLE contexts ADD COLUMN promoted_at INTEGER",
            "ALTER TABLE contexts ADD COLUMN demoted_at INTEGER",
            "ALTER TABLE contexts ADD COLUMN paused_at INTEGER",
            "ALTER TABLE tracks ADD COLUMN deleted_at INTEGER",
            "ALTER TABLE hooks ADD COLUMN action_ask_description TEXT",
            "ALTER TABLE contexts ADD COLUMN origin_host TEXT",
            "ALTER TABLE hooks ADD COLUMN action_kaish_path TEXT",
            "ALTER TABLE context_usage ADD COLUMN cache_ttl_secs INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE turns ADD COLUMN cache_write_1h_tokens INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE backends ADD COLUMN idle_timeout_secs INTEGER \
                 CHECK (idle_timeout_secs IS NULL OR idle_timeout_secs > 0)",
            "ALTER TABLE backends ADD COLUMN max_concurrent INTEGER \
                 CHECK (max_concurrent IS NULL OR max_concurrent > 0)",
            "ALTER TABLE characters ADD COLUMN handoff_ctx BLOB REFERENCES contexts(context_id) ON DELETE SET NULL",
            "ALTER TABLE characters ADD COLUMN root INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE characters ADD COLUMN root_ctx BLOB REFERENCES contexts(context_id) ON DELETE SET NULL",
            "ALTER TABLE characters ADD COLUMN context_type TEXT",
            "ALTER TABLE backend_models ADD COLUMN max_tokens INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0)",
            "ALTER TABLE backend_models ADD COLUMN temperature REAL \
                 CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0))",
            "ALTER TABLE backend_models ADD COLUMN top_p REAL CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0))",
            "ALTER TABLE backend_models ADD COLUMN effort TEXT",
            "ALTER TABLE backend_models ADD COLUMN thinking_budget INTEGER \
                 CHECK (thinking_budget IS NULL OR thinking_budget > 0)",
            "ALTER TABLE backend_models ADD COLUMN thinking_style TEXT",
        ];
        for sql in alters {
            match conn.execute(sql, []) {
                Ok(_) => {}
                Err(e) if is_duplicate_column_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        // Accountability moved from the sheet to the context forest, so the
        // column that held it goes. A drop is the one shape the additive
        // ladder above cannot express, and it is guarded on the column
        // still being there: a fresh database never had it.
        let has_accountable_to: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('characters') WHERE name = 'accountable_to')",
            [], |row| row.get(0),
        )?;
        if has_accountable_to {
            conn.execute("ALTER TABLE characters DROP COLUMN accountable_to", [])?;
            tracing::info!("dropped characters.accountable_to; accountability is a relation between contexts");
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS context_continuations (
                context_id BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
                epoch INTEGER NOT NULL,
                opened_at INTEGER NOT NULL,
                last_request_at INTEGER,
                yielded_at INTEGER,
                signed_off_at INTEGER,
                interrupted_at INTEGER,
                interrupted_by BLOB
            )",
        )?;
        for sql in [
            "ALTER TABLE context_continuations ADD COLUMN last_request_at INTEGER",
            "ALTER TABLE context_continuations ADD COLUMN interrupted_at INTEGER",
            "ALTER TABLE context_continuations ADD COLUMN interrupted_by BLOB",
        ] {
            match conn.execute(sql, []) {
                Ok(_) => {}
                Err(error) if is_duplicate_column_error(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Self::migrate_preset_cast_narrowing(conn)?;
        Self::drop_legacy_value_enum_checks(conn)?;
        Self::migrate_well_known_lost_found(conn)?;
        Self::migrate_doc_kind_file_collapse(conn)?;
        Self::migrate_archived_context_state(conn)?;
        Self::migrate_label_index_live_only(conn)?;
        Self::migrate_subtree_fork_kind_clear(conn)?;
        Self::drop_doc_snapshots_content_column(conn)?;
        Self::drop_consent_mode_columns(conn)?;
        // Review has no configured default; the lineage root holds that
        // authority (`docs/approval-identity.md`).
        conn.execute_batch("DROP TABLE IF EXISTS approval_identity_config")?;
        Self::remove_retired_classifier(conn)?;
        Self::migrate_house_capability(conn)?;
        if pre_context_character {
            Self::ensure_legacy_cast_tables(conn)?;
            Self::ensure_pre_context_character_columns(conn)?;
            Self::migrate_senpai_edges(conn)?;
            Self::drop_old_routing(conn)?;
        }
        Self::drop_structural_edges(conn)?;
        Self::ensure_context_character_guards(conn)?;
        if pre_context_character {
            Self::migrate_context_characters(conn)?;
            Self::drop_pre_context_character_columns(conn)?;
        }
        Self::migrate_character_slots(conn)?;
        Ok(())
    }

    /// The pre-character-slot cast tables, created empty when a database old
    /// enough to lack them is about to be read by the migrations that carry
    /// casts forward. Only called on a database that still has
    /// `contexts.provider`.
    fn ensure_legacy_cast_tables(conn: &Connection) -> KernelDbResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS casts (
                cast_id     BLOB NOT NULL PRIMARY KEY,
                label       TEXT NOT NULL UNIQUE COLLATE NOCASE,
                description TEXT,
                created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                created_by  BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS cast_slots (
                cast_id         BLOB NOT NULL REFERENCES casts(cast_id) ON DELETE CASCADE,
                role            TEXT NOT NULL,
                backend_id      BLOB NOT NULL REFERENCES backends(backend_id),
                model           TEXT NOT NULL CHECK (model != ''),
                max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
                temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
                top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
                effort          TEXT,
                thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
                thinking_style  TEXT,
                loadout         TEXT,
                extra           TEXT,
                PRIMARY KEY (cast_id, role)
            );",
        )?;
        Ok(())
    }

    /// Move each character's cast into `character_slots` and drop the casts
    /// (`docs/senpai-kohai.md`, "Migration", step 1), in one transaction.
    ///
    /// A character gets a copy of the slots of the cast it names; a character
    /// with no cast gets none. Every value that does not carry over is logged
    /// before it goes: a cast no character names, with its slots, and a
    /// slot's `loadout` and `extra`, which had no reader. Then
    /// `characters.cast_id`, `cast_slots` and `casts` are dropped. A database
    /// with neither cast table is on the new shape and does nothing, and the
    /// presence of the tables is the whole state: the move writes no marker. One that
    /// still has `characters.default_cast_id` has not been through
    /// `context-characters-v1` and refuses, because that migration reads the
    /// casts.
    fn migrate_character_slots(conn: &Connection) -> KernelDbResult<()> {
        if !Self::has_table(conn, "casts")? && !Self::has_table(conn, "cast_slots")? {
            return Ok(());
        }
        if Self::has_column(conn, "characters", "default_cast_id")? || Self::has_column(conn, "contexts", "cast_id")? {
            return Err(KernelDbError::Validation(
                "the casts cannot move to character slots before the context character migration has read them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        let has_cast_column = Self::has_column(&tx, "characters", "cast_id")?;
        if has_cast_column && Self::has_table(&tx, "cast_slots")? {
            tx.execute(
                "INSERT INTO character_slots
                     (principal_id, role, backend_id, model, max_tokens, temperature, top_p, effort,
                      thinking_budget, thinking_style)
                 SELECT c.principal_id, s.role, s.backend_id, s.model, s.max_tokens, s.temperature, s.top_p,
                        s.effort, s.thinking_budget, s.thinking_style
                 FROM characters c JOIN cast_slots s ON s.cast_id = c.cast_id
                 WHERE true
                 ON CONFLICT(principal_id, role) DO NOTHING",
                [],
            )?;
        }
        if Self::has_table(&tx, "casts")? {
            let named = if has_cast_column {
                "EXISTS (SELECT 1 FROM characters ch WHERE ch.cast_id = c.cast_id)"
            } else {
                "0"
            };
            let mut stmt = tx.prepare(&format!(
                "SELECT c.label, (SELECT COUNT(*) FROM cast_slots s WHERE s.cast_id = c.cast_id), {named} FROM casts c ORDER BY c.label"
            ))?;
            let casts: Vec<(String, i64, bool)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?
                .collect::<SqliteResult<_>>()?;
            for (label, slots, named) in casts {
                if !named {
                    warn!(cast = %label, slots, "dropping a cast no character names, with its slots");
                }
            }
        }
        if Self::has_table(&tx, "cast_slots")? {
            let mut stmt = tx.prepare(
                "SELECT c.label, s.role FROM cast_slots s JOIN casts c ON c.cast_id = s.cast_id
                 WHERE s.loadout IS NOT NULL OR s.extra IS NOT NULL ORDER BY c.label, s.role",
            )?;
            let unread: Vec<(String, String)> =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<SqliteResult<_>>()?;
            for (label, role) in unread {
                warn!(cast = %label, role = %role, "dropping a slot's loadout and extra, which nothing read");
            }
        }
        if has_cast_column {
            // A character that plays a live context with nothing to copy now
            // has no slots; the first create or rotate for it will refuse
            // naming the role, so say so here, where the deploy happens.
            let mut stmt = tx.prepare(
                "SELECT DISTINCT ch.name FROM characters ch
                 JOIN context_characters cc ON cc.principal_id = ch.principal_id
                 JOIN contexts c ON c.context_id = cc.context_id AND c.archived_at IS NULL
                 WHERE NOT EXISTS (SELECT 1 FROM character_slots s WHERE s.principal_id = ch.principal_id)
                 ORDER BY ch.name",
            )?;
            let bare: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<SqliteResult<_>>()?;
            drop(stmt);
            for name in bare {
                warn!(character = %name, "plays live contexts and has no slot after the casts moved; `kj character slot set` gives it one before the next create or rotate");
            }
            tx.execute("ALTER TABLE characters DROP COLUMN cast_id", [])?;
        }
        tx.execute("DROP TABLE IF EXISTS cast_slots", [])?;
        tx.execute("DROP TABLE IF EXISTS casts", [])?;
        tx.commit()?;
        info!("moved each character's cast into its own slots and dropped the casts");
        Ok(())
    }

    /// Add the pre-context-character columns a very old database lacks, so
    /// the two migrations that read them find them. Only called on a database
    /// that still has `contexts.provider`.
    fn ensure_pre_context_character_columns(conn: &Connection) -> KernelDbResult<()> {
        for (table, column, definition) in PRE_CONTEXT_CHARACTER_COLUMNS {
            if !Self::has_column(conn, table, column)? {
                conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"), [])?;
            }
        }
        Ok(())
    }

    /// Remove the retired risk classifier's global pre_call hook row and the
    /// per-context env its rc exported. Runs after the column backfills,
    /// because the hook match names `action_kaish_path`.
    fn remove_retired_classifier(conn: &Connection) -> KernelDbResult<()> {
        let (hook_id, hook_path) = RETIRED_CLASSIFIER_HOOK;
        let removed = conn.execute(
            "DELETE FROM hooks WHERE hook_id = ?1 AND action_kind = 'kaish_path' AND action_kaish_path = ?2",
            [hook_id, hook_path],
        )?;
        if removed > 0 {
            tracing::warn!(hook_id, "removed the retired classifier pre_call hook");
        }
        for key in RETIRED_CLASSIFIER_ENV {
            let removed = conn.execute("DELETE FROM context_env WHERE key = ?1", [key])?;
            if removed > 0 {
                tracing::info!(key, contexts = removed, "removed a retired classifier env var");
            }
        }
        Ok(())
    }

    /// Drops the unread `contexts.consent_mode` and `presets.consent_mode`
    /// columns so an older database keeps loading; guarded on
    /// `PRAGMA table_info` so a fresh database, whose `CREATE TABLE` never
    /// had the column, does nothing and this stays safe to call on every
    /// open.
    fn drop_consent_mode_columns(conn: &Connection) -> KernelDbResult<()> {
        for table in ["contexts", "presets"] {
            let has_column = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<SqliteResult<Vec<String>>>()?
                .iter()
                .any(|name| name == "consent_mode");
            if has_column {
                conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN consent_mode;"))?;
            }
        }
        Ok(())
    }

    /// `doc_snapshots.content` held a document's plain text beside `state`,
    /// which is the encoded whole document and already contains it. Nothing
    /// read the column: the single reader returns the row whole and every
    /// caller uses `state`, and a `state` that fails to decode skips the
    /// document rather than falling back to the text — so it was never the
    /// only readable copy. It cost about a fifth of the database.
    ///
    /// Guarded on `PRAGMA table_info` so a fresh database, which never had
    /// the column, does nothing and this stays safe to call on every open.
    /// `doc_snapshots` carries no trigger, no index and no child table, so
    /// dropping a column needs none of the rebuild machinery the value-enum
    /// `CHECK`s did.
    ///
    /// Dropping a column rewrites the table's rows but does not return pages
    /// to the filesystem. A `VACUUM` does, and that is an operator's call:
    /// running one at boot against a database this size would hold the kernel
    /// off its own startup for as long as the rewrite takes.
    fn drop_doc_snapshots_content_column(conn: &Connection) -> KernelDbResult<()> {
        let has_column = conn
            .prepare("PRAGMA table_info(doc_snapshots)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<SqliteResult<Vec<String>>>()?
            .iter()
            .any(|name| name == "content");
        if has_column {
            conn.execute_batch("ALTER TABLE doc_snapshots DROP COLUMN content;")?;
        }
        Ok(())
    }

    /// Drop a value-enum `CHECK` SQLite cannot widen in place, rather than
    /// grow it — extends the ruling against `approval_events.kind`'s CHECK
    /// (`approval-ledger/src/schema.rs`) to every closed-enum column in this
    /// file. A value-enum CHECK gates one point in the type's history; the
    /// next value always needs one more open-ended migration, and the type
    /// that already owns the enum (`BackendKind`, `RosterEntity`,
    /// `LivenessKind`, `Availability`) is the durable place to keep gating
    /// it. `SCHEMA`'s `CREATE TABLE IF NOT EXISTS` only ever reaches a fresh
    /// database, so every one of these needs the same rebuild a fresh
    /// database's schema already reflects.
    ///
    /// Drops:
    /// - `backends.kind`
    /// - `roster_entity.entity_kind`
    /// - `roster_presence.liveness_kind`
    /// - `roster_activity.live`
    /// - `roster_status.availability`
    ///
    /// `roster_entity` is the FK parent of `roster_presence` and
    /// `roster_status` (`ON DELETE CASCADE`), and `roster_presence` is in
    /// turn the parent of `roster_activity`, also cascade. With foreign keys
    /// enforced, `DROP TABLE roster_entity` performs an implicit `DELETE
    /// FROM` that fires every one of those edges and empties the live
    /// roster. `PRAGMA foreign_keys` is a no-op inside a transaction, so it
    /// is turned off before the rebuilds' shared transaction starts and
    /// restored to whatever it was on entry once that transaction has
    /// committed or rolled back — on every return path, including the error
    /// one. `PRAGMA foreign_key_check` runs before the commit as a second
    /// line of defense: any row it reports fails the whole rebuild loudly
    /// rather than committing a database missing rows it once had.
    ///
    /// Each rebuild is independently guarded (see
    /// [`Self::rebuild_table_dropping_check`]), so a database already on the
    /// checkless shape does none of them and this is a no-op safe to call on
    /// every open. `DROP TABLE` takes a table's indexes and triggers with
    /// it, so a rebuild re-runs `SCHEMA` afterward — every statement there
    /// is `IF NOT EXISTS`, so this restores `idx_roster_presence_entity` and
    /// `roster_entity_context_must_exist` and no-ops on everything else.
    fn drop_legacy_value_enum_checks(conn: &Connection) -> KernelDbResult<()> {
        let foreign_keys_were_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let rebuilt = Self::rebuild_value_enum_tables(conn);
        let restore_sql =
            if foreign_keys_were_on != 0 { "PRAGMA foreign_keys = ON;" } else { "PRAGMA foreign_keys = OFF;" };
        let restored = conn.execute_batch(restore_sql);

        let rebuilt = match (rebuilt, restored) {
            (Ok(rebuilt), Ok(())) => rebuilt,
            (Err(e), _) => return Err(e),
            (Ok(_), Err(e)) => return Err(e.into()),
        };
        if rebuilt {
            conn.execute_batch(SCHEMA)?;
        }
        Ok(())
    }

    /// Run every value-enum `CHECK` rebuild inside one transaction, so a
    /// mid-list failure leaves every table on whichever shape it already
    /// had rather than half-migrated. Returns whether any table actually
    /// rebuilt.
    fn rebuild_value_enum_tables(conn: &Connection) -> KernelDbResult<bool> {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let outcome = (|| -> KernelDbResult<bool> {
            let mut rebuilt = false;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "backends",
                "CHECK (kind IN (",
                "backend_id           BLOB NOT NULL PRIMARY KEY,
                 name                 TEXT NOT NULL UNIQUE,
                 kind                 TEXT NOT NULL,
                 base_url             TEXT,
                 api_key_env          TEXT,
                 api_key_file         TEXT,
                 key_optional         INTEGER NOT NULL DEFAULT 0,
                 request_timeout_secs INTEGER
                     CHECK (request_timeout_secs IS NULL OR request_timeout_secs > 0),
                 idle_timeout_secs    INTEGER
                     CHECK (idle_timeout_secs IS NULL OR idle_timeout_secs > 0),
                 max_concurrent       INTEGER
                     CHECK (max_concurrent IS NULL OR max_concurrent > 0),
                 created_at           INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 created_by           BLOB NOT NULL",
                "backend_id, name, kind, base_url, api_key_env, api_key_file, \
                 key_optional, request_timeout_secs, idle_timeout_secs, max_concurrent, created_at, created_by",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_entity",
                "CHECK (entity_kind IN (",
                "entity_kind   TEXT NOT NULL,
                 entity_id     BLOB NOT NULL,
                 label         TEXT,
                 first_seen_at INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 PRIMARY KEY (entity_kind, entity_id)",
                "entity_kind, entity_id, label, first_seen_at",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_presence",
                "CHECK (liveness_kind IN (",
                "source          TEXT NOT NULL,
                 source_local_id TEXT NOT NULL,
                 entity_kind     TEXT NOT NULL,
                 entity_id       BLOB NOT NULL,
                 liveness_kind   TEXT NOT NULL,
                 host            TEXT,
                 pid             INTEGER,
                 proc_start      INTEGER,
                 first_seen_at   INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE",
                "source, source_local_id, entity_kind, entity_id, liveness_kind, \
                 host, pid, proc_start, first_seen_at",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_activity",
                "CHECK (live IN (",
                "source          TEXT    NOT NULL,
                 source_local_id TEXT    NOT NULL,
                 live            INTEGER NOT NULL,
                 observed_at     INTEGER,
                 recorded_at     INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (source, source_local_id)
                     REFERENCES roster_presence(source, source_local_id) ON DELETE CASCADE",
                "source, source_local_id, live, observed_at, recorded_at",
            )?;
            rebuilt |= Self::rebuild_table_dropping_check(
                conn,
                "roster_status",
                "CHECK (availability IN (",
                "entity_kind  TEXT NOT NULL,
                 entity_id    BLOB NOT NULL,
                 status_text  TEXT,
                 availability TEXT NOT NULL,
                 observed_at  INTEGER,
                 recorded_at  INTEGER NOT NULL,
                 PRIMARY KEY (entity_kind, entity_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE",
                "entity_kind, entity_id, status_text, availability, observed_at, recorded_at",
            )?;

            let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                return Err(KernelDbError::Validation(format!(
                    "dropping the value-enum CHECK left a dangling foreign key on {table}; \
                     refusing to commit the rebuild"
                )));
            }
            Ok(rebuilt)
        })();

        match &outcome {
            Ok(_) => conn.execute_batch("COMMIT;")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK;");
            }
        }
        outcome
    }

    /// Rebuild `table` to drop a legacy value-enum `CHECK` SQLite cannot
    /// alter in place: copy every row into a same-shaped table without the
    /// CHECK, drop the original, and rename the copy into its place.
    ///
    /// Guarded on `check_marker`, a substring unique to the retired CHECK
    /// clause — never the bare word `"CHECK"`, since a table can carry
    /// other CHECKs (`backends.request_timeout_secs`) that must survive
    /// this rebuild untouched. A table already on the checkless shape has
    /// no occurrence of `check_marker` in its stored SQL, so this is a
    /// no-op and safe to call on every open.
    ///
    /// `column_defs` is the rebuilt table's full column/key/FK list
    /// (everything that would sit between the parens of a `CREATE TABLE`);
    /// `shared_columns` is the comma-separated column list common to both
    /// shapes, used for the `INSERT ... SELECT` that carries every row
    /// across. Foreign key enforcement and the surrounding transaction are
    /// the caller's responsibility.
    ///
    /// Returns whether it rebuilt. A caller running this over several
    /// tables uses that to decide whether `DROP TABLE` took an index or
    /// trigger with it that now needs restoring.
    fn rebuild_table_dropping_check(
        conn: &Connection,
        table: &str,
        check_marker: &str,
        column_defs: &str,
        shared_columns: &str,
    ) -> KernelDbResult<bool> {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .optional()?;
        let Some(sql) = sql else { return Ok(false) };
        if !sql.contains(check_marker) {
            return Ok(false);
        }
        let staging = format!("{table}__check_drop");
        conn.execute_batch(&format!(
            "CREATE TABLE {staging} (\n{column_defs}\n);
             INSERT INTO {staging} ({shared_columns})
                 SELECT {shared_columns} FROM {table};
             DROP TABLE {table};
             ALTER TABLE {staging} RENAME TO {table};",
        ))?;
        Ok(true)
    }

    /// One-shot preset narrowing for Track D (2026-08-03): a preset's old
    /// `provider`/`model` columns are dropped and replaced with a nullable
    /// `cast_id` reference (a preset now assigns a cast rather than pinning a
    /// provider/model pair itself). No attempt is made to guess a cast from
    /// the old provider/model — Amy: "a preset whose provider/model can't map
    /// just loses them (cast_id stays NULL)" — but every preset that's about
    /// to lose a non-null value logs it at `info` first, so the drop is
    /// never silent.
    ///
    /// Column presence is checked via `PRAGMA table_info` rather than the
    /// duplicate-column-error idiom `apply_additive_migrations` uses for
    /// ADD-only alters: this function does three independent operations
    /// (drop, drop, add) and needs to resume correctly even if a prior run
    /// crashed between them, which a single try-and-ignore-the-error pass
    /// over all three wouldn't detect per-column.
    fn migrate_preset_cast_narrowing(conn: &Connection) -> KernelDbResult<()> {
        let columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(presets)")?;
            stmt.query_map([], |r| r.get::<_, String>(1))?
                .collect::<SqliteResult<_>>()?
        };
        let has_provider = columns.iter().any(|c| c == "provider");
        let has_model = columns.iter().any(|c| c == "model");

        if has_provider || has_model {
            let provider_expr = if has_provider { "provider" } else { "NULL" };
            let model_expr = if has_model { "model" } else { "NULL" };
            let sql = format!(
                "SELECT label, {provider_expr}, {model_expr} FROM presets \
                 WHERE {provider_expr} IS NOT NULL OR {model_expr} IS NOT NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (label, provider, model) = row?;
                info!(
                    "preset '{label}': dropping provider={provider:?} model={model:?} \
                     (cast_id left NULL — no automatic mapping)"
                );
            }
        }

        if has_provider {
            conn.execute("ALTER TABLE presets DROP COLUMN provider", [])?;
        }
        if has_model {
            conn.execute("ALTER TABLE presets DROP COLUMN model", [])?;
        }
        Ok(())
    }

    /// One-time adoption of the pre-registry lost+found scheme
    /// (docs/drifting-dead-letters.md, slice 1): a database that predates
    /// `well_known_contexts` identified lost+found purely by the magic label
    /// `'lost+found'`. This backfills the registry row from whatever context
    /// currently holds that label, so `WellKnownRole::LostFound` lookups work
    /// on an upgraded database without anyone having to notice or act.
    ///
    /// `INSERT OR IGNORE` makes this idempotent across every `open()` — once
    /// a row exists for the role (whether from this backfill or a later
    /// rotation), the label stops being meaningful and this is a no-op
    /// forever after, even if the label is later reused or moved.
    fn migrate_well_known_lost_found(conn: &Connection) -> KernelDbResult<()> {
        conn.execute(
            "INSERT OR IGNORE INTO well_known_contexts (role, context_id, assigned_at)
             SELECT 'lost_found', context_id, ?1
               FROM contexts WHERE label = 'lost+found'
               ORDER BY created_at LIMIT 1",
            params![now_millis()],
        )?;
        Ok(())
    }

    /// Collapse the retired `code`, `text`, and `config` document kinds into
    /// the one `file` kind. The three tags described a single-block,
    /// path-addressed document whose content is the file body; the `language`
    /// column already carried whatever distinguished them.
    ///
    /// Idempotent by construction — after the first run the `WHERE` clause
    /// matches zero rows, so this needs no `kernel_migrations` marker and is
    /// safe on every `open()`.
    fn migrate_doc_kind_file_collapse(conn: &Connection) -> KernelDbResult<()> {
        conn.execute(
            "UPDATE documents SET doc_kind = 'file'
              WHERE doc_kind IN ('code', 'text', 'config')",
            [],
        )?;
        Ok(())
    }

    /// Bring `context_state` into agreement with `archived_at` on rows written
    /// before archiving set the state. Until then `archive_context` stamped
    /// only the timestamp, so every archived context read back as `live` and a
    /// caller switching on the enum alone saw a dead context as a running one.
    ///
    /// Idempotent by construction — after the first run the `WHERE` clause
    /// matches zero rows — so this needs no `kernel_migrations` marker and is
    /// safe on every `open()`.
    /// The label index covers live contexts only (`SCHEMA`,
    /// `idx_contexts_label`). A DB created when it also covered archived
    /// rows keeps that index — `CREATE INDEX IF NOT EXISTS` never replaces
    /// one — so it is rebuilt here when its SQL lacks the archived filter.
    /// Idempotent; a fresh DB matches and nothing runs.
    fn migrate_label_index_live_only(conn: &Connection) -> KernelDbResult<()> {
        let current: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_contexts_label'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let live_only = current
            .as_deref()
            .is_some_and(|sql| sql.contains("archived_at IS NULL"));
        if live_only {
            return Ok(());
        }
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_contexts_label;
             CREATE UNIQUE INDEX idx_contexts_label
                 ON contexts(label) WHERE label IS NOT NULL AND archived_at IS NULL;",
        )?;
        Ok(())
    }

    fn migrate_archived_context_state(conn: &Connection) -> KernelDbResult<()> {
        conn.execute(
            "UPDATE contexts SET context_state = 'archived'
              WHERE archived_at IS NOT NULL AND context_state <> 'archived'",
            [],
        )?;
        Ok(())
    }

    /// Clear the retired `subtree` fork kind from stored rows. The
    /// template-subtree fork variant is gone from `ForkKind`
    /// (`docs/fork-and-create.md`), and `fork_kind_from_sql` treats any
    /// unrecognized value as corruption rather than degrading silently —
    /// so a row a deleted template-subtree fork left behind must lose the
    /// tag before `get_context` (or any other reader) can read it back.
    ///
    /// Idempotent by construction — after the first run the `WHERE` clause
    /// matches zero rows — so this needs no `kernel_migrations` marker and
    /// is safe on every `open()`.
    fn migrate_subtree_fork_kind_clear(conn: &Connection) -> KernelDbResult<()> {
        let cleared = conn.execute("UPDATE contexts SET fork_kind = NULL WHERE fork_kind = 'subtree'", [])?;
        if cleared > 0 {
            info!(cleared, "cleared retired 'subtree' fork_kind from stored contexts");
        }
        Ok(())
    }

    const SENPAI_MIGRATION: &'static str = "senpai-edges-v1";

    /// Set `senpai_ctx` for every context from what routed it before the
    /// senpai edge (`docs/senpai-kohai.md`, "Migration"), once. The old
    /// columns and tables are read where they still exist;
    /// [`Self::drop_old_routing`] removes them after this has run.
    ///
    /// For a context C with a parent on `forked_from`, run once as the old
    /// resolver would have, acting as C's performer:
    ///
    /// - With an explicit reviewer override, or else the director's active
    ///   delegation, the reviewer R is that character. The senpai is the
    ///   nearest strict ancestor of C whose holder is R. With none, it is R's
    ///   root console when R is a root, else R's most recent live context
    ///   other than C; either is reported.
    /// - Otherwise the old walk: the nearest strict ancestor whose holder
    ///   (performer, else director) is not C's performer. A retired holder
    ///   there is mapped to that ancestor too, so the new resolver refuses by
    ///   name exactly as the old one did.
    /// - A context with no parent and no override is a top.
    ///
    /// A live context that fits none of these refuses the whole migration,
    /// naming each one; nothing is written. An archived one is left a top and
    /// reported instead, since it answers no asks.
    fn migrate_senpai_edges(conn: &Connection) -> KernelDbResult<()> {
        use std::collections::{BTreeMap, HashMap};
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::SENPAI_MIGRATION], |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;

        let contexts: Vec<OldContext> = {
            let mut stmt = tx.prepare(
                "SELECT context_id, label, created_at, forked_from, archived_at, played_by
                 FROM contexts ORDER BY created_at, context_id",
            )?;
            stmt.query_map([], |r| Ok(OldContext {
                context_id: read_context_id(r, 0)?,
                label: r.get(1)?,
                created_at: r.get(2)?,
                forked_from: read_opt_context_id(r, 3)?,
                archived_at: r.get(4)?,
                played_by: read_opt_principal_id(r, 5)?,
            }))?.collect::<SqliteResult<_>>()?
        };
        let characters: HashMap<PrincipalId, CharacterRow> = {
            let mut stmt = tx.prepare(
                "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, NULL FROM characters",
            )?;
            stmt.query_map([], row_to_character_row)?
                .map(|r| r.map(|c| (c.principal_id, c)))
                .collect::<SqliteResult<_>>()?
        };
        // The old routing, where this database still has it. A table with
        // `reviewer_id` but no `director_id` predates the director column, and
        // the open that added it cleared every override, so none survive.
        let has_reviewer = Self::has_column(&tx, "contexts", "reviewer_id")?;
        let has_director = Self::has_column(&tx, "contexts", "director_id")?;
        let old_routing: HashMap<ContextId, (Option<PrincipalId>, Option<PrincipalId>)> = if has_director {
            let reviewer = if has_reviewer { "reviewer_id" } else { "NULL" };
            let mut stmt = tx.prepare(&format!("SELECT context_id, {reviewer}, director_id FROM contexts"))?;
            stmt.query_map([], |r| Ok((read_context_id(r, 0)?, (read_opt_principal_id(r, 1)?, read_opt_principal_id(r, 2)?))))?
                .collect::<SqliteResult<_>>()?
        } else {
            HashMap::new()
        };
        let delegations: HashMap<PrincipalId, PrincipalId> = if Self::has_table(&tx, "approval_reviewer_delegations")? {
            let mut stmt = tx.prepare(
                "SELECT director_id, reviewer_id FROM approval_reviewer_delegations WHERE revoked_at IS NULL",
            )?;
            stmt.query_map([], |r| Ok((read_principal_id(r, 0)?, read_principal_id(r, 1)?)))?
                .collect::<SqliteResult<_>>()?
        } else {
            HashMap::new()
        };
        let by_id: HashMap<ContextId, &OldContext> = contexts.iter().map(|c| (c.context_id, c)).collect();
        let root_owner: HashMap<ContextId, PrincipalId> = characters.values()
            .filter_map(|c| c.root_ctx.map(|ctx| (ctx, c.principal_id))).collect();
        let holder = |c: &OldContext| c.played_by.or_else(|| root_owner.get(&c.context_id).copied());
        let name_of = |p: PrincipalId| characters.get(&p).map_or_else(|| p.short(), |c| c.name.clone());
        let describe = |c: &OldContext| c.label.as_ref().map_or_else(
            || c.context_id.short(), |l| format!("{l} ({})", c.context_id.short()),
        );

        enum Plan {
            Top,
            Senpai { senpai: ContextId, note: Option<String>, old_reviewer: Option<PrincipalId> },
            Unmapped(String),
        }

        // The strict ancestors of `c` on `forked_from`, nearest first.
        let ancestors = |c: &OldContext| -> Result<Vec<&OldContext>, String> {
            let mut chain = Vec::new();
            let mut seen = HashSet::from([c.context_id]);
            let mut next = c.forked_from;
            while let Some(id) = next {
                if !seen.insert(id) {
                    return Err(format!("its forked_from chain has a cycle at {}", id.short()));
                }
                let Some(parent) = by_id.get(&id) else {
                    return Err(format!("its forked_from names context {}, which has no row", id.short()));
                };
                chain.push(*parent);
                next = parent.forked_from;
            }
            Ok(chain)
        };

        let plan = |c: &OldContext| -> Plan {
            let (reviewer_id, director_id) = old_routing.get(&c.context_id).copied().unwrap_or((None, None));
            let override_reviewer = reviewer_id.map(|r| (r, "explicit reviewer override".to_string())).or_else(|| {
                let director = director_id?;
                delegations.get(&director).map(|r| (*r, format!("delegation from director {}", name_of(director))))
            });
            if c.forked_from.is_none() && override_reviewer.is_none() {
                return Plan::Top;
            }
            let chain = match ancestors(c) {
                Ok(chain) => chain,
                Err(why) => return Plan::Unmapped(why),
            };
            if let Some((reviewer, source)) = override_reviewer {
                match characters.get(&reviewer) {
                    None => return Plan::Unmapped(format!("its {source} names a character with no sheet")),
                    Some(r) if r.retired_at.is_some() => return Plan::Unmapped(format!(
                        "its {source} names {}, who is retired", r.name
                    )),
                    Some(_) => {}
                }
                if let Some(ancestor) = chain.iter().find(|a| holder(a) == Some(reviewer)) {
                    return Plan::Senpai { senpai: ancestor.context_id, note: None, old_reviewer: Some(reviewer) };
                }
                let who = name_of(reviewer);
                let sheet = &characters[&reviewer];
                let console = sheet.root.then_some(sheet.root_ctx).flatten()
                    .and_then(|ctx| by_id.get(&ctx))
                    .filter(|r| r.archived_at.is_none() && r.context_id != c.context_id);
                if let Some(console) = console {
                    return Plan::Senpai {
                        senpai: console.context_id,
                        note: Some(format!(
                            "its {source} named {who}, who holds nothing above it: mapped to {who}'s root console; \
                             set its senpai again with `kj context move`"
                        )),
                        old_reviewer: Some(reviewer),
                    };
                }
                let recent = contexts.iter()
                    .filter(|o| o.archived_at.is_none() && o.played_by == Some(reviewer) && o.context_id != c.context_id)
                    .max_by_key(|o| (o.created_at, o.context_id));
                return match recent {
                    Some(recent) => Plan::Senpai {
                        senpai: recent.context_id,
                        note: Some(format!(
                            "its {source} named {who}, who holds nothing above it: mapped to {who}'s most recent live context; \
                             set its senpai again with `kj context move`"
                        )),
                        old_reviewer: Some(reviewer),
                    },
                    None => Plan::Unmapped(format!(
                        "its {source} names {who}, who holds nothing above it, has no root console and plays no live context"
                    )),
                };
            }
            // The old walk, acting as the performer (or as nobody).
            for ancestor in chain {
                if let Some(h) = holder(ancestor) && Some(h) != c.played_by {
                    return Plan::Senpai { senpai: ancestor.context_id, note: None, old_reviewer: Some(h) };
                }
            }
            Plan::Top
        };

        let mut assigned: BTreeMap<ContextId, Option<ContextId>> = BTreeMap::new();
        let mut report: Vec<(ContextId, Option<ContextId>, String, Option<PrincipalId>)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for c in &contexts {
            match plan(c) {
                Plan::Top => { assigned.insert(c.context_id, None); }
                Plan::Senpai { senpai, note, old_reviewer } => {
                    assigned.insert(c.context_id, Some(senpai));
                    if let Some(note) = note {
                        report.push((c.context_id, Some(senpai), note, old_reviewer));
                    }
                }
                Plan::Unmapped(why) if c.archived_at.is_some() => {
                    assigned.insert(c.context_id, None);
                    report.push((c.context_id, None, format!(
                        "archived and left without a senpai: {why}"
                    ), None));
                }
                Plan::Unmapped(why) => refused.push(format!("{}: {why}", describe(c))),
            }
        }
        // A fallback can point anywhere; a loop would make routing refuse forever.
        for start in assigned.keys() {
            let mut seen = HashSet::from([*start]);
            let mut at = assigned[start];
            while let Some(next) = at {
                if !seen.insert(next) {
                    refused.push(format!(
                        "{}: its mapped senpai chain loops back through {}",
                        by_id.get(start).map_or_else(|| start.short(), |c| describe(c)), next.short()
                    ));
                    break;
                }
                at = assigned.get(&next).copied().flatten();
            }
        }
        if !refused.is_empty() {
            refused.sort();
            refused.dedup();
            return Err(KernelDbError::Validation(format!(
                "the senpai migration cannot map {} live context{} and will not guess; nothing was changed. \
                 Give each a reachable reviewer, or clear its override, with the previous kernel, then start again:\n  {}",
                refused.len(), if refused.len() == 1 { "" } else { "s" }, refused.join("\n  ")
            )));
        }

        let now = now_millis() as i64;
        for (context, senpai) in &assigned {
            tx.execute(
                "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
                params![senpai.as_ref().map(|id| id.as_bytes().to_vec()), blob_param(context.as_bytes())],
            )?;
        }
        for (context, senpai, note, old_reviewer) in &report {
            tx.execute(
                "INSERT OR REPLACE INTO senpai_migration_report (context_id, senpai_ctx, note, old_reviewer, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    blob_param(context.as_bytes()),
                    senpai.as_ref().map(|id| id.as_bytes().to_vec()),
                    note,
                    old_reviewer.as_ref().map(|id| id.as_bytes().to_vec()),
                    now,
                ],
            )?;
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::SENPAI_MIGRATION])?;
        tx.commit()?;
        info!(
            mapped = assigned.values().filter(|s| s.is_some()).count(),
            reported = report.len(),
            "mapped every context to its senpai"
        );
        Ok(())
    }

    fn has_column(conn: &Connection, table: &str, column: &str) -> KernelDbResult<bool> {
        Ok(conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
            [column], |row| row.get(0),
        )?)
    }

    fn has_table(conn: &Connection, table: &str) -> KernelDbResult<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table], |row| row.get(0),
        )?)
    }

    /// Drop the routing the senpai edge replaced: `contexts.reviewer_id`,
    /// `contexts.director_id`, and the delegation table and its events
    /// (`docs/senpai-kohai.md`, "Removed"). Runs only once
    /// [`Self::migrate_senpai_edges`] has recorded its mapping, which read
    /// them; a mapping that refused leaves them for the next open. What they
    /// said survives as each context's `senpai_ctx`, and, where that was not
    /// the nearest ancestor, as the old reviewer in `senpai_migration_report`.
    /// A fresh database never had them and does nothing.
    fn drop_old_routing(conn: &Connection) -> KernelDbResult<()> {
        let mapped: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::SENPAI_MIGRATION], |row| row.get(0),
        )?;
        if !mapped {
            return Err(KernelDbError::Validation(
                "the old routing columns cannot be dropped before the senpai migration has mapped them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        let mut dropped = Vec::new();
        for column in ["reviewer_id", "director_id"] {
            if Self::has_column(&tx, "contexts", column)? {
                tx.execute(&format!("ALTER TABLE contexts DROP COLUMN {column}"), [])?;
                dropped.push(format!("contexts.{column}"));
            }
        }
        for table in ["approval_reviewer_delegation_events", "approval_reviewer_delegations"] {
            if Self::has_table(&tx, table)? {
                tx.execute(&format!("DROP TABLE {table}"), [])?;
                dropped.push(table.to_string());
            }
        }
        tx.commit()?;
        if !dropped.is_empty() {
            info!(dropped = %dropped.join(", "), "dropped the routing the senpai edge replaced");
        }
        Ok(())
    }

    /// Drop what context characters replaced (`docs/senpai-kohai.md`,
    /// "Removed"), in one transaction: `contexts.{senpai_ctx, played_by,
    /// cast_id, provider, model}`, `presets.{cast_id, system_prompt}` and the
    /// table `llm_defaults`; and rename `characters.default_cast_id` to
    /// `characters.cast_id`. Runs only once
    /// [`Self::migrate_context_characters`] has recorded its mapping, which read them;
    /// a mapping that refused leaves them for the next open.
    ///
    /// No schema this kernel has created puts one of these columns under an
    /// index, trigger, view, `CHECK` or table-level foreign key, so each is a
    /// plain `DROP COLUMN`, and SQLite refuses loudly if a hand-made one is
    /// there; a column-level `REFERENCES` goes with its column. Each step is
    /// guarded by what is still there, so a database already on the new shape
    /// does nothing. The preset values are logged before they go;
    /// `senpai_migration_report` stays, because `kj context info` prints its
    /// notes until the senpai is set again.
    fn drop_pre_context_character_columns(conn: &Connection) -> KernelDbResult<()> {
        let mapped: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::CONTEXT_CHARACTERS_MIGRATION], |row| row.get(0),
        )?;
        if !mapped {
            return Err(KernelDbError::Validation(
                "the pre-context-character columns cannot be dropped before the context character migration has mapped them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        if Self::has_column(&tx, "presets", "cast_id")? || Self::has_column(&tx, "presets", "system_prompt")? {
            let cast = if Self::has_column(&tx, "presets", "cast_id")? { "cast_id IS NOT NULL" } else { "0" };
            let prompt = if Self::has_column(&tx, "presets", "system_prompt")? { "system_prompt IS NOT NULL" } else { "0" };
            let mut stmt = tx.prepare(&format!("SELECT label FROM presets WHERE {cast} OR {prompt} ORDER BY label"))?;
            let labels: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<SqliteResult<_>>()?;
            for label in labels {
                info!(preset = %label, "dropping the cast and system prompt of a preset; a preset keeps only its fork filters");
            }
        }
        let mut dropped = Vec::new();
        for (table, column) in [
            ("contexts", "senpai_ctx"),
            ("contexts", "played_by"),
            ("contexts", "cast_id"),
            ("contexts", "provider"),
            ("contexts", "model"),
            ("presets", "cast_id"),
            ("presets", "system_prompt"),
        ] {
            if Self::has_column(&tx, table, column)? {
                tx.execute(&format!("ALTER TABLE {table} DROP COLUMN {column}"), [])?;
                dropped.push(format!("{table}.{column}"));
            }
        }
        if Self::has_column(&tx, "characters", "default_cast_id")? {
            tx.execute("ALTER TABLE characters RENAME COLUMN default_cast_id TO cast_id", [])?;
            dropped.push("characters.default_cast_id (renamed to cast_id)".to_string());
        }
        if Self::has_table(&tx, "llm_defaults")? {
            tx.execute("DROP TABLE llm_defaults", [])?;
            dropped.push("llm_defaults".to_string());
        }
        tx.commit()?;
        if !dropped.is_empty() {
            info!(dropped = %dropped.join(", "), "dropped what context characters replaced");
        }
        Ok(())
    }

    /// Remove the `kind = 'structural'` rows of `context_edges`, which
    /// duplicated `contexts.forked_from`, and the `kind` column with them.
    ///
    /// The column's presence is the state: a database without it is done.
    /// Before deleting, every structural row is compared with `forked_from`.
    /// A row whose target was not forked from its source, a target with two
    /// structural parents, and a kind that is neither `structural` nor
    /// `drift` each refuse the whole migration, naming the contexts; nothing
    /// is written. The refusal repeats on every open until the rows are
    /// corrected by hand.
    fn drop_structural_edges(conn: &Connection) -> KernelDbResult<()> {
        if !Self::has_column(conn, "context_edges", "kind")? {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let mut refusals = Vec::new();

        let unknown: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT kind FROM context_edges WHERE kind NOT IN ('structural', 'drift')",
            )?;
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<SqliteResult<_>>()?
        };
        for kind in unknown {
            refusals.push(format!("an edge has the unknown kind '{kind}'"));
        }

        let rows: Vec<(ContextId, ContextId, Option<ContextId>)> = {
            let mut stmt = tx.prepare(
                "SELECT e.source_id, e.target_id, c.forked_from
                 FROM context_edges e
                 JOIN contexts c ON c.context_id = e.target_id
                 WHERE e.kind = 'structural'
                 ORDER BY e.target_id, e.source_id",
            )?;
            stmt.query_map([], |r| {
                Ok((read_context_id(r, 0)?, read_context_id(r, 1)?, read_opt_context_id(r, 2)?))
            })?
            .collect::<SqliteResult<_>>()?
        };
        let mut parents: HashMap<ContextId, Vec<ContextId>> = HashMap::new();
        for (source, target, forked_from) in &rows {
            parents.entry(*target).or_default().push(*source);
            if *forked_from != Some(*source) {
                refusals.push(format!(
                    "context {} has structural parent {} but forked_from {}",
                    target.short(),
                    source.short(),
                    forked_from.map_or_else(|| "none".to_string(), |p| p.short()),
                ));
            }
        }
        for (target, sources) in &parents {
            if sources.len() > 1 {
                let names: Vec<String> = sources.iter().map(|s| s.short()).collect();
                refusals.push(format!(
                    "context {} has {} structural parents ({})",
                    target.short(),
                    sources.len(),
                    names.join(", "),
                ));
            }
        }
        if !refusals.is_empty() {
            refusals.sort();
            refusals.dedup();
            return Err(KernelDbError::Validation(format!(
                "cannot drop the structural context_edges rows: {}. Correct forked_from or the edge rows; \
                 forked_from is the only parent link.",
                refusals.join("; "),
            )));
        }

        let deleted = tx.execute("DELETE FROM context_edges WHERE kind = 'structural'", [])?;
        tx.execute_batch(
            "DROP INDEX IF EXISTS idx_edges_structural_unique;
             ALTER TABLE context_edges DROP COLUMN kind;",
        )?;
        tx.commit()?;
        info!(deleted, "dropped the structural context_edges rows; forked_from is the only parent link");
        Ok(())
    }

    /// Contexts whose senpai the migration could not take from the nearest
    /// ancestor: `(context, the senpai it chose, the note)`. `kj context info`
    /// prints the note until the senpai is set again.
    pub fn senpai_migration_report(&self) -> KernelDbResult<Vec<(ContextId, Option<ContextId>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, senpai_ctx, note FROM senpai_migration_report ORDER BY recorded_at, context_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((read_context_id(r, 0)?, read_opt_context_id(r, 1)?, r.get::<_, String>(2)?)))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    const HOUSE_MIGRATION: &'static str = "grant-house-capability-v1";

    /// Grant `house` once to every binding that existed before the
    /// capability did, so an upgraded kernel keeps the administration verbs
    /// on its contexts. Worker types (coder, toolie, musician) are the seats
    /// the capability narrows, so they do not get it; a binding that grants
    /// nothing stays empty, because `house` alone would make it look usable.
    ///
    /// Recorded in `kernel_migrations`: a seat that later drops `house` must
    /// not get it back on the next open. A fresh database records the marker
    /// with nothing to grant.
    fn migrate_house_capability(conn: &Connection) -> KernelDbResult<()> {
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::HOUSE_MIGRATION], |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let granted = tx.execute(
            "INSERT OR IGNORE INTO context_binding_authorities (context_id, authority)
             SELECT b.context_id, 'house'
             FROM context_bindings b JOIN contexts c ON c.context_id = b.context_id
             WHERE c.context_type NOT IN ('coder', 'toolie', 'musician')
               AND (b.all_instances OR b.all_facades OR b.binding_admin
                    OR EXISTS (SELECT 1 FROM context_binding_instances WHERE context_id = b.context_id)
                    OR EXISTS (SELECT 1 FROM context_binding_tools WHERE context_id = b.context_id)
                    OR EXISTS (SELECT 1 FROM context_binding_facades WHERE context_id = b.context_id)
                    OR EXISTS (SELECT 1 FROM context_binding_authorities WHERE context_id = b.context_id))",
            [],
        )?;
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::HOUSE_MIGRATION])?;
        tx.commit()?;
        if granted > 0 {
            info!(granted, "granted the house capability to existing non-worker bindings");
        }
        Ok(())
    }

    /// Open or create at the given path.
    ///
    /// The DB schema (`SCHEMA`) is the source of truth for a new database;
    /// [`Self::apply_additive_migrations`] brings an older one up to it with
    /// guarded steps (additive columns, and the named migrations and drops
    /// that each guard on what is still there). A database still on casts is
    /// copied aside first ([`Self::back_up_before_character_slots`]). rc
    /// lifecycle scripts are no longer table rows — they live as files under
    /// `/config/rc` (see `seed_scripts` and `rc/mod.rs`), seeded to disk
    /// at server boot.
    pub fn open<P: AsRef<Path>>(path: P) -> KernelDbResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        Self::init_connection(&conn)?;
        Self::back_up_before_character_slots(&conn, path)?;
        conn.execute_batch(SCHEMA)?;
        Self::apply_additive_migrations(&conn)?;
        Self::migrate_ledger(&conn)?;
        Self::ensure_singleton_kernel(&conn)?;
        Ok(Self {
            conn,
            #[cfg(any(test, feature = "test-util"))]
            _temp_dir: None,
        })
    }

    /// Where a database still on casts is copied before `character-slots-v1`
    /// drops them: `kernel.db.pre-character-slots`, beside the database.
    fn pre_character_slots_backup_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".pre-character-slots");
        path.with_file_name(name)
    }

    /// Copy a database that still has its casts aside before the migration
    /// that drops them runs. The copy is a consistent snapshot
    /// (`VACUUM INTO`). A copy that already exists is the state before an
    /// earlier attempt and stays as it is. When the copy cannot be made the
    /// open fails and says why, and nothing is migrated.
    fn back_up_before_character_slots(conn: &Connection, path: &Path) -> KernelDbResult<()> {
        if !Self::has_table(conn, "casts")? && !Self::has_table(conn, "cast_slots")? {
            return Ok(());
        }
        let backup = Self::pre_character_slots_backup_path(path);
        if backup.exists() {
            info!(backup = %backup.display(), "a pre-character-slots backup already exists; keeping it");
            return Ok(());
        }
        conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()]).map_err(|e| {
            KernelDbError::Validation(format!(
                "the casts become character slots on this open, and the copy of the database at {} could not be made: {e}; \
                 free the space or the path, or stop the server that holds the database, and start again",
                backup.display()
            ))
        })?;
        info!(backup = %backup.display(), "copied the database aside before the casts become character slots");
        Ok(())
    }

    /// Open an existing `kernel.db` as a genuine read-only SQLite connection:
    /// no `SCHEMA` execution, no migration. `open()` always runs both, which
    /// needs write access even when nothing changes — unusable for a reader
    /// that must never contend with a live server's write connection.
    ///
    /// The lockout-recovery CLI (`kaijutsu-server list-characters`) and
    /// `add-key --as`'s name lookup: both read `kernel.db` while a server
    /// may already be holding it open, and both must work with the service
    /// stopped. Fails loudly if the file does not exist or is not a
    /// kaijutsu `kernel.db` — there is nothing sensible to fall back to.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> KernelDbResult<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn,
            #[cfg(any(test, feature = "test-util"))]
            _temp_dir: None,
        })
    }

    /// Create a real, file-backed database under a fresh throwaway directory,
    /// owned by the returned handle: the directory (and the `.db` file inside
    /// it) is removed when this `KernelDb` drops.
    ///
    /// Never `:memory:`. A `:memory:` connection is per-connection state that
    /// never touches a file, so two `in_memory()` handles used to be two
    /// separate, unrelated databases even though the name suggested shared
    /// state — and none of it exercised WAL mode, file locking, or reopening
    /// across a restart, all of which durability work needs to hit. `open()`
    /// and `temporary()` run the identical setup sequence (`SCHEMA`,
    /// migrations); the only difference is a real file under a directory
    /// nobody but this handle knows about.
    ///
    /// Test-only: gated behind `cfg(test)` for this crate's own tests and
    /// `feature = "test-util"` for integration tests and other crates
    /// (`kaijutsu-server`) that opt in — never reachable from production code.
    #[cfg(any(test, feature = "test-util"))]
    pub fn temporary() -> KernelDbResult<Self> {
        let dir = tempfile::tempdir().expect("create temporary kernel db directory");
        let mut db = Self::open(dir.path().join("kernel.db"))?;
        db._temp_dir = Some(dir);
        Ok(db)
    }

    /// Migrate the approval-ledger schema into the kernel DB. The ledger is
    /// DB-injected (`approval_ledger::migrate` borrows whatever connection
    /// the caller owns), so the kernel owns the single connection and the
    /// ledger's tables live beside the kernel's own — one DB, one backup,
    /// one WAL. `execute_batch`'s `CREATE TABLE IF NOT EXISTS` makes this
    /// idempotent across every open.
    fn migrate_ledger(conn: &Connection) -> KernelDbResult<()> {
        approval_ledger::migrate(conn)?;
        // Jump the ledger's change generation forward by a fixed gap, once
        // per DB open — which for the live kernel is once per process start,
        // i.e. exactly the restart boundary this is meant to mark.
        //
        // The counter is durable, so it is already monotonic without this;
        // the gap is insurance for the cases that break that promise anyway
        // (a crash mid-transaction, a database restored from a backup), where
        // a client could otherwise be handed a generation it already held
        // with different facts sitting behind it. It also makes a restart
        // legible to a subscriber as a discontinuity rather than a silent
        // resume.
        //
        // Deliberately here rather than inside `approval_ledger::migrate`:
        // that function is idempotent and documented as safe to call on every
        // process start, so folding the jump into it would make the jump size
        // depend on how many times it happened to run. A second `KernelDb`
        // open in one process would over-gap, and that is fine — an extra
        // discontinuity costs a subscriber one poll, while a *missing* one
        // could cost it a stale view it has no way to notice.
        //
        // Failing the open is the right response to a failure here, rather
        // than logging and continuing: the counter is the only thing telling
        // subscribers a restart happened, so a kernel that came up unable to
        // advance it would hand out change notifications indistinguishable
        // from the previous boot's.
        approval_ledger::generation::bump_for_restart(conn)?;
        Ok(())
    }

    // ========================================================================
    // Kernel identity
    // ========================================================================

    // ========================================================================
    // Auto-Workspaces
    // ========================================================================

    // ========================================================================
    // Document CRUD
    // ========================================================================

    // ========================================================================
    // Op-Log Persistence
    // ========================================================================

    /// Run `f` as one SQLite transaction on this connection. Commits when
    /// `f` returns `Ok`; an `Err` rolls back every write `f` made. Fails if a
    /// transaction is already open on the connection.
    pub fn in_transaction<T>(
        &self,
        f: impl FnOnce(&Self) -> KernelDbResult<T>,
    ) -> KernelDbResult<T> {
        let tx = self.conn.unchecked_transaction()?;
        let out = f(self)?;
        tx.commit()?;
        Ok(out)
    }

    /// Run a WAL checkpoint in TRUNCATE mode: flush committed WAL frames into
    /// the main database file and shrink the `-wal` file back to zero.
    ///
    /// Without proactive checkpoints the WAL grows between SQLite's automatic
    /// 1000-page checkpoints, and a bare-file read of the main `.db` lags
    /// behind committed history — the mechanism behind the 2026-06-11
    /// journaling-forensics misread (no data was lost; the main file was
    /// simply stale while ~4 MB of newer ops sat only in `kernel.db-wal`).
    /// Compaction calls this so the main file tracks the oplog.
    ///
    /// Returns `(busy, log_frames, checkpointed_frames)` as SQLite reports
    /// them. `busy == 1` means a concurrent reader/writer on another
    /// connection prevented a full truncate; the checkpoint is best-effort
    /// and the caller treats busy as non-fatal.
    pub fn checkpoint(&self) -> KernelDbResult<(i64, i64, i64)> {
        let row = self.conn.query_row(
            "PRAGMA wal_checkpoint(TRUNCATE)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(row)
    }

    /// Hot-backup this database into a fresh file at `path` via SQLite's
    /// native `VACUUM INTO` — a single consistent, already-compacted
    /// snapshot that's safe to run against a live writer (unlike a bare `cp`
    /// of `kernel.db`, which can race the `-wal` file and produce a torn or
    /// stale copy). No prior `checkpoint()` call is required — `VACUUM INTO`
    /// reads a consistent snapshot regardless of WAL state.
    ///
    /// The path is bound as a query parameter (`VACUUM INTO ?1`), not
    /// string-interpolated, so a path containing `'` or other SQL
    /// metacharacters can't break out of the statement.
    ///
    /// SQLite refuses to overwrite an existing target file — that error
    /// surfaces as-is (`KernelDbError::Db`) rather than being papered over by
    /// pre-deleting the target, so a caller can never silently clobber an
    /// existing backup by name collision.
    pub fn vacuum_into<P: AsRef<Path>>(&self, path: P) -> KernelDbResult<()> {
        let path_str = path.as_ref().to_str().ok_or_else(|| {
            KernelDbError::Validation(format!(
                "backup path is not valid UTF-8: {}",
                path.as_ref().display()
            ))
        })?;
        self.conn.execute("VACUUM INTO ?1", params![path_str])?;
        Ok(())
    }

    // ========================================================================
    // Context CRUD
    // ========================================================================

    /// Atomically create a forked context: the document row, the context row,
    /// the child's `cwd`, and the env + egress + capability-binding config
    /// copied from `source`, all in ONE transaction.
    ///
    /// `cwd` is the caller's: `kj fork` passes the source's cwd or `--pwd`
    /// after checking that `cd` enters it. `None` leaves the child without
    /// a cwd.
    ///
    /// This folds `insert_context_with_document` + `fork_context_config` into a
    /// single all-or-nothing write. Calling them separately left a gap: the
    /// context insert committed first, so a failure copying the config stranded
    /// a committed-but-misconfigured context (under deny-by-default, one with no
    /// loadout — locked out). Here a failure on any write rolls the whole fork
    /// back, leaving no context row behind.
    ///
    /// The source config is read up front (SELECTs) so the transaction holds
    /// only the writes, matching `fork_context_config`.
    pub fn insert_forked_context(
        &mut self,
        row: &ContextRow,
        default_workspace_id: WorkspaceId,
        source: ContextId,
        cwd: Option<&str>,
    ) -> KernelDbResult<()> {
        let env = self.get_context_env(source)?;
        let egress = self.list_context_egress(source)?;
        // A parent's context-scoped MCP grants name servers declared for the
        // parent alone; the child keeps the rest of the loadout.
        let binding = self
            .get_context_binding(source)?
            .map(crate::mcp::ContextToolBinding::without_context_scoped);

        let ws_id = row.workspace_id.unwrap_or(default_workspace_id);
        let doc = DocumentRow {
            document_id: row.context_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row.created_at,
            created_by: row.created_by,
        };

        let tx = self.conn.transaction()?;
        Self::write_document_or_ignore(&tx, &doc)?;
        Self::write_context(&tx, row)?;
        if let Some(cwd) = cwd {
            Self::write_context_shell(
                &tx,
                &ContextShellRow {
                    context_id: row.context_id,
                    cwd: Some(cwd.to_string()),
                    updated_at: now_millis(),
                },
            )?;
        }
        for var in &env {
            Self::write_context_env(&tx, row.context_id, &var.key, &var.value)?;
        }
        for host in &egress {
            Self::write_context_egress(&tx, row.context_id, host)?;
        }
        if let Some(binding) = binding {
            Self::write_binding(&tx, row.context_id, &binding)?;
        }
        // Attachments travel with the fork: the child joins the SAME tracks as the
        // parent (docs/tracks.md §3 — "The context binds; the child inherits the
        // bind at fork"). The child re-binds via create/fork rc on the way up so
        // the track never has to watch for forks. A non-musician parent has no
        // attachments — the copy is a clean no-op.
        Self::copy_attachments_for_fork(&tx, source, row.context_id)?;
        // The child is the same character as its source, answering to it.
        context_characters::copy_character_into(&tx, source, row.context_id)?;
        tx.commit()?;
        Ok(())
    }

    /// Test fixture: record that `context` should answer to `senpai`.
    /// [`Self::update_played_by`] gives the context character this senpai when
    /// it writes it, and a context that already has one is moved at once, so a
    /// fixture may build a chain in either order. The note lives in a
    /// temporary table of this connection.
    #[cfg(any(test, feature = "test-util"))]
    pub fn note_senpai_intent(&self, context: ContextId, senpai: Option<ContextId>) -> KernelDbResult<()> {
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS test_senpai_intent (context_id BLOB NOT NULL PRIMARY KEY, senpai_ctx BLOB)",
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO test_senpai_intent (context_id, senpai_ctx) VALUES (?1, ?2)",
            params![blob_param(context.as_bytes()), senpai.as_ref().map(|id| id.as_bytes().to_vec())],
        )?;
        if self.context_character(context)?.is_some() {
            self.conn.execute(
                "UPDATE context_characters SET senpai_ctx = ?1 WHERE context_id = ?2",
                params![senpai.as_ref().map(|id| id.as_bytes().to_vec()), blob_param(context.as_bytes())],
            )?;
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-util"))]
    fn has_senpai_intent_table(&self) -> KernelDbResult<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_temp_master WHERE type = 'table' AND name = 'test_senpai_intent')",
            [], |r| r.get(0),
        )?)
    }

    #[cfg(any(test, feature = "test-util"))]
    fn senpai_intent(&self, context: ContextId) -> KernelDbResult<Option<ContextId>> {
        if !self.has_senpai_intent_table()? {
            return Ok(None);
        }
        Ok(self.conn
            .query_row(
                "SELECT senpai_ctx FROM test_senpai_intent WHERE context_id = ?1",
                params![blob_param(context.as_bytes())],
                |r| read_opt_context_id(r, 0),
            )
            .optional()?
            .flatten())
    }

    /// Test fixture: give `context` a context character of `played_by`, the
    /// way creation does, under the senpai [`Self::note_senpai_intent`] named.
    /// A context that already has this character's context character is left
    /// alone; a different character rewrites it (see
    /// [`Self::recast_test_character`]) and `None` refuses.
    #[cfg(any(test, feature = "test-util"))]
    pub fn update_played_by(
        &self,
        id: ContextId,
        played_by: Option<PrincipalId>,
    ) -> KernelDbResult<()> {
        let Some(played_by) = played_by else {
            return Err(KernelDbError::Validation(
                "a context character never changes its character, so a fixture cannot clear a performer".into(),
            ));
        };
        self.assign_context_character(id, played_by)?;
        Ok(())
    }

    /// Test fixture: rewrite the character of `id`'s context character to
    /// `played_by`. No kernel path does this: a context's character never
    /// changes. For wire tests that make a context through `kj context create`
    /// and then need another character to perform it. The trigger that fixes
    /// the principal is dropped for the update and put back.
    #[cfg(any(test, feature = "test-util"))]
    pub fn recast_test_character(&self, id: ContextId, played_by: PrincipalId) -> KernelDbResult<()> {
        self.context_character(id)?
            .ok_or_else(|| KernelDbError::Validation(format!("context {} has no context character to recast", id.short())))?;
        self.conn.execute_batch("DROP TRIGGER IF EXISTS context_characters_fixed")?;
        let updated = self.conn.execute(
            "UPDATE context_characters SET principal_id = ?1 WHERE context_id = ?2",
            params![blob_param(played_by.as_bytes()), blob_param(id.as_bytes())],
        );
        Self::ensure_context_character_guards(&self.conn)?;
        updated?;
        Ok(())
    }

    /// [`Self::assign_test_review`] after recasting a context character of
    /// another character to `played_by` (see [`Self::recast_test_character`]).
    #[cfg(any(test, feature = "test-util"))]
    pub fn assign_test_review_recasting(
        &self,
        context: ContextId,
        played_by: Option<PrincipalId>,
        reviewer: Option<PrincipalId>,
    ) -> KernelDbResult<()> {
        if let (Some(played_by), Some(character)) = (played_by, self.context_character(context)?)
            && character.principal_id != played_by
        {
            self.recast_test_character(context, played_by)?;
        }
        self.assign_test_review(context, played_by, reviewer)
    }

    /// Test fixture: make `character` run `model` on `backend` for each of
    /// `roles`. See [`Self::seed_test_slot_entries`].
    #[cfg(any(test, feature = "test-util"))]
    pub fn seed_test_slots(
        &self,
        character: PrincipalId,
        backend: &str,
        model: &str,
        roles: &[&str],
    ) -> KernelDbResult<()> {
        let entries: Vec<(&str, &str, &str)> = roles.iter().map(|role| (*role, backend, model)).collect();
        self.seed_test_slot_entries(character, &entries)
    }

    /// Test fixture: give `character` exactly these slots, one per
    /// `(role, backend, model)`, replacing the slots it had. A backend row is
    /// created (kind `mock`, no key) when absent, and each slot sets
    /// `max_tokens`. A context character written after this copies the slots;
    /// one written before keeps the slots it has.
    #[cfg(any(test, feature = "test-util"))]
    pub fn seed_test_slot_entries(
        &self,
        character: PrincipalId,
        entries: &[(&str, &str, &str)],
    ) -> KernelDbResult<()> {
        for existing in self.list_character_slots(character)? {
            self.delete_character_slot(character, &existing.role)?;
        }
        for (role, backend, model) in entries {
            let backend_row = match self.get_backend_by_name(backend)? {
                Some(row) => row,
                None => {
                    let row = BackendRow {
                        backend_id: BackendId::new(),
                        name: backend.to_string(),
                        kind: "mock".to_string(),
                        base_url: None,
                        api_key_env: None,
                        api_key_file: None,
                        key_optional: true,
                        request_timeout_secs: None,
                        idle_timeout_secs: None,
                        max_concurrent: None,
                        created_at: 0,
                        created_by: PrincipalId::system(),
                    };
                    self.upsert_backend(&row)?;
                    row
                }
            };
            self.set_character_slot(&CharacterSlotRow {
                principal_id: character,
                role: role.to_string(),
                backend_id: backend_row.backend_id,
                model: model.to_string(),
                max_tokens: Some(4096),
                temperature: None,
                top_p: None,
                effort: None,
                thinking_budget: None,
                thinking_style: None,
            })?;
        }
        Ok(())
    }

    /// Test fixture: give `id` a stand-in context character of `played_by`,
    /// which a later [`Self::update_played_by`] or [`Self::assign_test_review`]
    /// naming another character replaces. For helpers that register a context
    /// before a test says who runs it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn assign_stand_in_character(&self, id: ContextId, played_by: PrincipalId) -> KernelDbResult<()> {
        let existed = self.context_character(id)?.is_some();
        self.assign_context_character(id, played_by)?;
        if !existed {
            self.conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS test_stand_in_character (context_id BLOB NOT NULL PRIMARY KEY)",
            )?;
            self.conn.execute(
                "INSERT OR IGNORE INTO test_stand_in_character (context_id) VALUES (?1)",
                params![blob_param(id.as_bytes())],
            )?;
        }
        Ok(())
    }

    /// Whether `id`'s context character is a stand-in a fixture may replace.
    #[cfg(any(test, feature = "test-util"))]
    fn is_stand_in_character(&self, id: ContextId) -> KernelDbResult<bool> {
        let table: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_temp_master WHERE type = 'table' AND name = 'test_stand_in_character')",
            [], |r| r.get(0),
        )?;
        if !table {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM test_stand_in_character WHERE context_id = ?1)",
            params![blob_param(id.as_bytes())],
            |r| r.get(0),
        )?)
    }

    /// Write `id`'s context character as creation does. A root character's
    /// first context with no senpai becomes its console. An existing context
    /// character of another character is rewritten.
    #[cfg(any(test, feature = "test-util"))]
    fn assign_context_character(&self, id: ContextId, played_by: PrincipalId) -> KernelDbResult<ContextCharacterRow> {
        self.get_context(id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", id.short())))?;
        if let Some(existing) = self.context_character(id)? {
            if existing.principal_id == played_by {
                return Ok(existing);
            }
            if !self.is_stand_in_character(id)? {
                return Err(KernelDbError::Validation(format!(
                    "context {} already has a context character of another character, which never changes",
                    id.short()
                )));
            }
            self.recast_test_character(id, played_by)?;
            self.conn.execute(
                "DELETE FROM test_stand_in_character WHERE context_id = ?1",
                params![blob_param(id.as_bytes())],
            )?;
            return Ok(self.require_context_character(id)?);
        }
        let senpai = self.senpai_intent(id)?;
        let row = self.write_context_character(id, played_by, senpai, None)?;
        if senpai.is_none()
            && let Some(sheet) = self.get_character(played_by)?
            && sheet.root
            && sheet.root_ctx.is_none()
        {
            self.set_character_root_ctx(played_by, Some(id))?;
        }
        Ok(row)
    }

    /// Test fixture: make `context` performed by `played_by` and reviewed by
    /// `reviewer`: the reviewer holds a seat context (created on first use)
    /// with its own context character, and the context is placed under the
    /// seat, so routing follows the senpai edge like production. With no
    /// reviewer only the performer is set; with no performer a stand-in
    /// character runs a context that has no context character yet.
    #[cfg(any(test, feature = "test-util"))]
    pub fn assign_test_review(
        &self,
        context: ContextId,
        played_by: Option<PrincipalId>,
        reviewer: Option<PrincipalId>,
    ) -> KernelDbResult<()> {
        // A context routes through its context character, so one with no
        // named performer is run by a stand-in character.
        match (played_by, self.context_character(context)?) {
            (Some(played_by), _) => { self.assign_context_character(context, played_by)?; }
            (None, None) => { self.assign_context_character(context, PrincipalId::new())?; }
            (None, Some(_)) => {}
        }
        let Some(reviewer) = reviewer else { return Ok(()) };
        let label = format!("review-seat-{}", reviewer.to_hex());
        let seat = match self.find_context_by_label(&label)? {
            Some(seat) => seat.context_id,
            None => {
                let seat = ContextId::new();
                let ws = self.get_or_create_default_workspace(reviewer)?;
                let row = ContextRow {
                    context_id: seat, label: Some(label), system_prompt: None,
                    context_state: ContextState::Live, context_type: "default".into(),
                    created_at: now_millis(), created_by: reviewer, forked_from: None, fork_kind: None,
                    archived_at: None, workspace_id: None, preset_id: None, concluded_at: None,
                    last_activity_at: None, promoted_at: None, demoted_at: None, paused_at: None,
                    origin_host: None, played_by: None,
                };
                // The reviewer's own senpai is where the context answered
                // before, so the climb continues where the old walk did.
                let previous = self.senpai_of(context)?;
                self.insert_context_with_document(&row, ws)?;
                self.assign_context_character(seat, reviewer)?;
                if let Some(previous) = previous {
                    self.set_context_senpai(seat, Some(previous))?;
                }
                seat
            }
        };
        self.set_context_senpai(context, Some(seat))
    }

    // ========================================================================
    // Context Edges
    // ========================================================================

    // ========================================================================
    // Presets
    // ========================================================================

    // ========================================================================
    // Workspaces
    // ========================================================================

    // ========================================================================
    // Workspace Paths
    // ========================================================================

    // ========================================================================
    // Context Shell Configuration
    // ========================================================================

    // ========================================================================
    // Context Token Usage (token-usage gauge)
    // ========================================================================

    // ========================================================================
    // Turns (per-turn provenance and spend)
    // ========================================================================

    // ========================================================================
    // Context Tool Bindings (Phase 5, D-54)
    // ========================================================================

    /// Force every subsequent write on this connection to fail with
    /// SQLite's read-only error, without touching schema or data — used to
    /// prove a write path leaves prior state untouched when the write
    /// itself fails partway. Never compiled outside test builds.
    #[cfg(test)]
    pub(crate) fn set_query_only_for_test(&self, on: bool) -> KernelDbResult<()> {
        let pragma = if on { "PRAGMA query_only = ON;" } else { "PRAGMA query_only = OFF;" };
        self.conn.execute_batch(pragma)?;
        Ok(())
    }

    // ========================================================================
    // Hook Persistence
    // ========================================================================

    // ========================================================================
    // Hook scripts (kernel-global shared kaish bodies)
    // ========================================================================

    // ========================================================================
    // Context Environment Variables
    // ========================================================================

    // ========================================================================
    // Context egress (docs/egress.md)
    // ========================================================================

    // ========================================================================
    // Claude cache breakpoints (per-context policy)
    // ========================================================================

}

impl KernelDb {
    // ========================================================================
    // Tracks (clock domains — docs/tracks.md Stage 1)
    // ========================================================================

    // ========================================================================
    // Attachments (context → track binding — docs/tracks.md §2+3)
    // ========================================================================

    // ========================================================================
    // Context Config Fork + Workspace Query
    // ========================================================================

    /// Copy shell config + env vars + egress hosts + capability binding from
    /// source context to target. Called during all fork operations. The
    /// binding copy makes permissions follow the fork — under deny-by-default
    /// a fork would otherwise start with no loadout and be locked out. Grants
    /// for MCP servers declared for the source alone are not copied.
    ///
    /// Atomic: the four copies land in ONE transaction. A fork that fails
    /// partway must leave NO partial config behind — a half-copied loadout
    /// would lock the fork out (or grant a stale subset), and the caller has
    /// already committed the context row, so a silent partial here would
    /// strand a misconfigured context. The source is read up front so the
    /// transaction holds only the writes.
    pub fn fork_context_config(&mut self, source: ContextId, target: ContextId) -> KernelDbResult<()> {
        let shell = self.get_context_shell(source)?;
        let env = self.get_context_env(source)?;
        let egress = self.list_context_egress(source)?;
        let binding = self
            .get_context_binding(source)?
            .map(crate::mcp::ContextToolBinding::without_context_scoped);

        let tx = self.conn.transaction()?;
        if let Some(src) = shell {
            Self::write_context_shell(
                &tx,
                &ContextShellRow {
                    context_id: target,
                    cwd: src.cwd,
                    updated_at: now_millis(),
                },
            )?;
        }
        for var in &env {
            Self::write_context_env(&tx, target, &var.key, &var.value)?;
        }
        for host in &egress {
            Self::write_context_egress(&tx, target, host)?;
        }
        if let Some(binding) = binding {
            Self::write_binding(&tx, target, &binding)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Copy the capability binding (flags + instances + tools + facades +
    /// sticky names) from `source` to `target`. No-op if the source has no
    /// binding. The child can later attenuate (self-narrow) but inherits the
    /// parent's loadout as its starting point, without the parent's
    /// context-scoped MCP server grants.
    pub fn copy_context_binding(
        &mut self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        match self.get_context_binding(source)? {
            Some(binding) => {
                self.upsert_context_binding(target, &binding.without_context_scoped())?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ========================================================================
    // Workspace Permission Checking
    // ========================================================================

    // ========================================================================
    // Phase 4A: Additional methods for kj commands
    // ========================================================================

    // ========================================================================
    // Well-Known Context Registry (docs/drifting-dead-letters.md, slice 1)
    // ========================================================================

    // ========================================================================
    // Client Views (docs/shared-state.md "Retiring KV")
    // ========================================================================

    // ========================================================================
    // Dirty File Buffers (docs/file-buffers.md — swap-file marker)
    // ========================================================================

}

// ============================================================================
// LLM configuration (backends / models / casts / aliases / defaults)
// ============================================================================
//
// SQL is the source of truth for model configuration — there is no TOML in
// this path. Every write validates loudly and every failure bubbles: a
// swallowed config write means the kernel talks to the wrong model, which is
// exactly the silent-wrong-answer this project refuses.

/// The retired risk classifier's global pre_call hook: its id and the body
/// path its rc installed. Opening a DB removes a row matching both.
pub(crate) const RETIRED_CLASSIFIER_HOOK: (&str, &str) = ("lfm2d-advisory", "/config/rc/lib/hooks/lfm2d.kai");

/// Per-context env keys the retired classifier's rc exported. Opening a DB
/// removes them; nothing reads them.
pub(crate) const RETIRED_CLASSIFIER_ENV: &[&str] = &["LFM2D_MODE", "LFM2D_BENIGN_LABEL", "LFM2D_URL"];

fn read_backend_id(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<BackendId> {
    let bytes: Vec<u8> = row.get(idx)?;
    BackendId::try_from_slice(&bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "backend_id blob is not 16 bytes",
            )),
        )
    })
}


impl KernelDb {
    // ── backends ────────────────────────────────────────────────────────

    // ── backend_models ──────────────────────────────────────────────────

    // ── character slots ─────────────────────────────────────────────────

    // ── character models ────────────────────────────────────────────────

    // ── characters ──────────────────────────────────────────────────────

    // ── model_aliases ───────────────────────────────────────────────────

    // ── embedding_config ────────────────────────────────────────────────

}

/// The `thinking_style` tokens the providers accept; `llm/claude/build.rs`
/// refuses any other at turn build.
pub const THINKING_STYLES: [&str; 4] = ["off", "auto", "adaptive", "budget"];

/// Range-check the shared tunable knobs. Loud rejection, never a clamp — the
/// SQL CHECKs say the same thing, but a Rust-side error names the knob and
/// the legal range instead of surfacing `SQLITE_CONSTRAINT`.
fn validate_tunables(
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    thinking_budget: Option<i64>,
    thinking_style: Option<&str>,
) -> KernelDbResult<()> {
    if let Some(v) = max_tokens
        && v <= 0
    {
        return Err(KernelDbError::Validation(format!(
            "max_tokens must be a positive integer (got {v})"
        )));
    }
    if let Some(v) = temperature
        && !(0.0..=2.0).contains(&v)
    {
        return Err(KernelDbError::Validation(format!(
            "temperature must be in 0.0..=2.0 (got {v})"
        )));
    }
    if let Some(v) = top_p
        && !(v > 0.0 && v <= 1.0)
    {
        return Err(KernelDbError::Validation(format!(
            "top_p must be in (0.0, 1.0] (got {v})"
        )));
    }
    if let Some(v) = thinking_budget
        && v <= 0
    {
        return Err(KernelDbError::Validation(format!(
            "thinking_budget must be a positive integer (got {v})"
        )));
    }
    if let Some(v) = thinking_style
        && !THINKING_STYLES.contains(&v)
    {
        return Err(KernelDbError::Validation(format!(
            "thinking_style must be one of {} (got {v:?})",
            THINKING_STYLES.join(", ")
        )));
    }
    Ok(())
}

// ============================================================================
// Row parsers
// ============================================================================

fn row_to_document_row(row: &rusqlite::Row<'_>) -> SqliteResult<DocumentRow> {
    let kind_str: String = row.get(2)?;
    Ok(DocumentRow {
        document_id: read_context_id(row, 0)?,
        workspace_id: read_workspace_id(row, 1)?,
        doc_kind: doc_kind_from_sql(&kind_str),
        language: row.get(3)?,
        path: row.get(4)?,
        created_at: row.get(5)?,
        created_by: read_principal_id(row, 6)?,
    })
}

fn row_to_context_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextRow> {
    let state_str: String = row.get(3)?;
    let fork_kind_str: Option<String> = row.get(8)?;

    Ok(ContextRow {
        context_id: read_context_id(row, 0)?,
        label: row.get(1)?,
        system_prompt: row.get(2)?,
        context_state: context_state_from_sql(&state_str),
        context_type: row.get(4)?,
        created_at: row.get(5)?,
        created_by: read_principal_id(row, 6)?,
        forked_from: read_opt_context_id(row, 7)?,
        fork_kind: fork_kind_from_sql(fork_kind_str)?,
        archived_at: row.get(9)?,
        workspace_id: read_opt_workspace_id(row, 10)?,
        preset_id: read_opt_preset_id(row, 11)?,
        concluded_at: row.get(12)?,
        last_activity_at: row.get(13)?,
        promoted_at: row.get(14)?,
        demoted_at: row.get(15)?,
        paused_at: row.get(16)?,
        origin_host: row.get(17)?,
        played_by: read_opt_principal_id(row, 18)?,
    })
}

fn row_to_edge_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextEdgeRow> {
    Ok(ContextEdgeRow {
        edge_id: read_edge_id(row, 0)?,
        source_id: read_context_id(row, 1)?,
        target_id: read_context_id(row, 2)?,
        metadata: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn row_to_preset_row(row: &rusqlite::Row<'_>) -> SqliteResult<PresetRow> {
    Ok(PresetRow {
        preset_id: read_preset_id(row, 0)?,
        label: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        created_by: read_principal_id(row, 4)?,
    })
}

fn row_to_character_row(row: &rusqlite::Row<'_>) -> SqliteResult<CharacterRow> {
    Ok(CharacterRow {
        principal_id: read_principal_id(row, 0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        retired_at: row.get(3)?,
        handoff_ctx: read_opt_context_id(row, 4)?,
        root: row.get(5)?,
        root_ctx: read_opt_context_id(row, 6)?,
        context_type: row.get(7)?,
    })
}

fn row_to_hook_script_row(row: &rusqlite::Row<'_>) -> SqliteResult<HookScriptRow> {
    Ok(HookScriptRow {
        script_id: row.get(0)?,
        body: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        created_by: read_principal_id(row, 4)?,
        updated_at: row.get(5)?,
    })
}

fn row_to_workspace_row(row: &rusqlite::Row<'_>) -> SqliteResult<WorkspaceRow> {
    Ok(WorkspaceRow {
        workspace_id: read_workspace_id(row, 0)?,
        label: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        created_by: read_principal_id(row, 4)?,
        archived_at: row.get(5)?,
    })
}

// ============================================================================
// Roster (crate::roster) — storage half; types/invariants live in roster.rs
// ============================================================================

impl KernelDb {
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A DB whose `hooks` table predates `action_kaish_path` still opens:
    /// the retired-hook cleanup runs after the column backfill it names.
    #[test]
    fn retired_classifier_cleanup_opens_a_hooks_table_without_kaish_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        {
            let db = KernelDb::open(&path).unwrap();
            db.conn.execute_batch("ALTER TABLE hooks DROP COLUMN action_kaish_path").unwrap();
        }
        let db = KernelDb::open(&path).expect("a legacy hooks table must still open");
        assert!(db.load_all_hooks().unwrap().is_empty());
    }

    /// The retired classifier's global pre_call hook row and the per-context
    /// env its rc exported are removed at open. A hook with the same id that
    /// runs a different body, and every other env key, stay.
    #[test]
    fn retired_classifier_hook_and_env_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        let (hook_id, hook_path) = RETIRED_CLASSIFIER_HOOK;
        let context_id = {
            let db = KernelDb::open(&path).unwrap();
            let ws_id = setup_test_db(&db);
            let ctx = make_context_row(Some("retired-classifier"));
            insert_context_with_doc(&db, &ctx, ws_id);
            let mut hook = minimal_hook_row(hook_id, "pre_call", 0);
            hook.match_tool = Some("shell_write".into());
            hook.action_kind = "kaish_path".into();
            hook.action_kaish_path = Some(hook_path.into());
            hook.action_log_target = None;
            hook.action_log_level = None;
            db.insert_hook(&hook).unwrap();
            db.insert_hook(&minimal_hook_row("operator-hook", "pre_call", 0)).unwrap();
            for key in RETIRED_CLASSIFIER_ENV {
                db.set_context_env(ctx.context_id, key, "escalate").unwrap();
            }
            db.set_context_env(ctx.context_id, "EDITOR", "vim").unwrap();
            ctx.context_id
        };
        let db = KernelDb::open(&path).unwrap();
        let hooks: Vec<String> = db.load_all_hooks().unwrap().into_iter().map(|h| h.hook_id).collect();
        assert_eq!(hooks, vec!["operator-hook".to_string()]);
        let env: Vec<String> = db.get_context_env(context_id).unwrap().into_iter().map(|e| e.key).collect();
        assert_eq!(env, vec!["EDITOR".to_string()]);
        drop(db);

        let db = KernelDb::open(&path).unwrap();
        let mut same_id_other_body = minimal_hook_row(hook_id, "pre_call", 0);
        same_id_other_body.action_kind = "kaish_path".into();
        same_id_other_body.action_kaish_path = Some("/config/rc/lib/hooks/mine.kai".into());
        db.insert_hook(&same_id_other_body).unwrap();
        drop(db);
        assert_eq!(KernelDb::open(&path).unwrap().load_all_hooks().unwrap().len(), 2);
    }

    #[test]
    fn fork_kind_from_sql_fails_loud_on_unknown() {
        // NULL column → None; known value parses.
        assert!(fork_kind_from_sql(None).unwrap().is_none());
        assert_eq!(
            fork_kind_from_sql(Some("filtered".into())).unwrap(),
            Some(ForkKind::Filtered)
        );
        // A retired ('shallow', 'subtree') or corrupt value must ERROR, never
        // silently degrade to None — that would erase fork provenance.
        assert!(fork_kind_from_sql(Some("shallow".into())).is_err());
        assert!(fork_kind_from_sql(Some("subtree".into())).is_err());
        assert!(fork_kind_from_sql(Some("garbage".into())).is_err());
    }

    /// A row a deleted template-subtree fork left behind with
    /// `fork_kind = 'subtree'` must not survive a reopen —
    /// `fork_kind_from_sql` treats any unrecognized value as corruption, so
    /// `open()` clears the retired tag on every open before any reader can
    /// trip over it.
    #[test]
    fn open_normalizes_a_stored_subtree_fork_kind_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("kernel.db");
        let context_id;
        {
            let db = KernelDb::open(&db_path).unwrap();
            let ws_id = setup_test_db(&db);
            let row = make_context_row(Some("old-subtree-fork"));
            context_id = row.context_id;
            insert_context_with_doc(&db, &row, ws_id);
            db.conn
                .execute(
                    "UPDATE contexts SET fork_kind = 'subtree' WHERE context_id = ?1",
                    params![blob_param(context_id.as_bytes())],
                )
                .unwrap();
        }
        // Drop closed the connection; re-open the same file. Without the
        // migration this read fails: `get_context` runs `fork_kind_from_sql`
        // over the stored 'subtree' value and errors.
        let db = KernelDb::open(&db_path).unwrap();
        let loaded = db.get_context(context_id).unwrap().unwrap();
        assert_eq!(loaded.fork_kind, None);
    }

    // ── 1. Schema idempotent ────────────────────────────────────────────

    #[test]
    fn schema_idempotent() {
        let db = KernelDb::temporary().unwrap();
        // Apply schema again — should not error.
        db.conn.execute_batch(SCHEMA).unwrap();
    }

    /// The retired `gate_actions` / `gate_action_argv` tables are dropped by
    /// `SCHEMA` on every open, so a database that ran the old code sheds them
    /// rather than carrying debris no code knows about. Simulates that old
    /// database by hand (`gate_action_argv` first: it foreign-keys onto
    /// `gate_actions`) and asserts both are gone after the drop runs.
    ///
    /// Falsified by deleting the `DROP TABLE IF EXISTS gate_actions;`
    /// statement from `SCHEMA`: `gate_actions` survived (`gate_action_argv`
    /// still went, since its own DROP is independent). Reverted.
    #[test]
    fn retired_gate_action_tables_are_dropped_on_open() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute_batch(
                "CREATE TABLE gate_actions (request_id TEXT NOT NULL PRIMARY KEY);
                 CREATE TABLE gate_action_argv (
                     request_id TEXT NOT NULL REFERENCES gate_actions(request_id),
                     seq INTEGER NOT NULL,
                     arg TEXT NOT NULL,
                     PRIMARY KEY (request_id, seq)
                 );",
            )
            .unwrap();

        db.conn.execute_batch(SCHEMA).unwrap();

        let table_count = |name: &str| -> i64 {
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(table_count("gate_actions"), 0, "gate_actions must be dropped");
        assert_eq!(table_count("gate_action_argv"), 0, "gate_action_argv must be dropped");
    }

    /// A database carrying `backends`' pre-`codex-app` CHECK must lose it on
    /// the next `apply_additive_migrations` (the real "next open" path),
    /// keep the row it already had, and accept a `kind` the old CHECK
    /// rejected.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_pre_codex_backends_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        put_backends_back_on_its_pre_codex_shape(&db.conn);
        let kept_id = BackendId::new();
        db.conn
            .execute(
                &format!(
                    "INSERT INTO backends ({BACKENDS_LEGACY_COLS}) VALUES (?1, 'kept', 'anthropic', 0, 1000, ?2)"
                ),
                params![blob_param(kept_id.as_bytes()), blob_param(PrincipalId::system().as_bytes())],
            )
            .unwrap();

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'backends'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
        assert_eq!(db.get_backend_by_name("kept").unwrap().unwrap().kind, "anthropic");

        // `codex-app` is the kind whose arrival needed the CHECK widened in
        // the first place: the fixture's constraint rejects it, and
        // `BackendKind::parse` accepts it. Writing it here is what proves the
        // old constraint is gone rather than merely absent from `SCHEMA`.
        let codex = BackendRow {
            backend_id: BackendId::new(),
            name: "codex".into(),
            kind: "codex-app".into(),
            base_url: None,
            api_key_env: None,
            api_key_file: None,
            key_optional: false,
            request_timeout_secs: None,
            idle_timeout_secs: None,
            max_concurrent: Some(3),
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        };
        db.upsert_backend(&codex).unwrap();
        let stored = db.get_backend_by_name("codex").unwrap().unwrap();
        assert_eq!(stored.kind, "codex-app");
        // The rebuild carries the column the additive ladder added first.
        assert_eq!(stored.max_concurrent, Some(3));
    }

    /// `rebuild_table_dropping_check` guards on the retired CHECK's own text,
    /// never the bare word "CHECK" — `backends` keeps its
    /// `request_timeout_secs` CHECK, so a naive guard would call every open a
    /// legacy shape forever. First call on a legacy `backends` rebuilds and
    /// reports `true`; the second, against the now-checkless table, must
    /// report `false` without touching the table again. Falsified by
    /// guarding on the bare substring `"CHECK"` instead: the second call also
    /// returned `true` (`request_timeout_secs`'s CHECK is still there).
    #[test]
    fn rebuild_table_dropping_check_is_a_true_no_op_on_its_second_call() {
        let db = KernelDb::temporary().unwrap();
        put_backends_back_on_its_pre_codex_shape(&db.conn);

        let column_defs = "backend_id           BLOB NOT NULL PRIMARY KEY,
                 name                 TEXT NOT NULL UNIQUE,
                 kind                 TEXT NOT NULL,
                 base_url             TEXT,
                 api_key_env          TEXT,
                 api_key_file         TEXT,
                 key_optional         INTEGER NOT NULL DEFAULT 0,
                 request_timeout_secs INTEGER
                     CHECK (request_timeout_secs IS NULL OR request_timeout_secs > 0),
                 created_at           INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 created_by           BLOB NOT NULL";
        let shared_columns = "backend_id, name, kind, base_url, api_key_env, api_key_file, \
                 key_optional, request_timeout_secs, created_at, created_by";

        let first = KernelDb::rebuild_table_dropping_check(
            &db.conn,
            "backends",
            "CHECK (kind IN (",
            column_defs,
            shared_columns,
        )
        .unwrap();
        assert!(first, "the legacy shape must be rebuilt on the first call");

        let second = KernelDb::rebuild_table_dropping_check(
            &db.conn,
            "backends",
            "CHECK (kind IN (",
            column_defs,
            shared_columns,
        )
        .unwrap();
        assert!(!second, "an already-checkless table must not be rebuilt again");
    }

    /// Stage 1 (time-well): `last_activity_at` is added to `SCHEMA` directly
    /// AND listed in `apply_additive_migrations` for pre-migration DBs. On a
    /// fresh DB (column already in `SCHEMA`), re-running the migration must be
    /// a guarded no-op — the same idempotency contract as `concluded_at`.
    #[test]
    fn last_activity_at_migration_is_idempotent_no_op_on_fresh_db() {
        let db = KernelDb::temporary().unwrap();
        // temporary() already ran apply_additive_migrations() once; running it
        // again must not error (duplicate-column guard) and the column must
        // still be queryable.
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("mig-check"));
        insert_context_with_doc(&db, &row, ws_id);
        let loaded = db.get_context(row.context_id).unwrap().unwrap();
        assert_eq!(loaded.last_activity_at, None, "fresh row: never touched");
    }

    /// A database from before the director column has `reviewer_id` but no
    /// `director_id`. The open that added the director column cleared every
    /// such reviewer, so the senpai migration maps none of them as an
    /// override: the context takes its nearest ancestor and is not reported.
    /// The drop then removes `reviewer_id`.
    #[test]
    fn a_reviewer_from_before_the_director_column_is_not_an_override() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
        into_pre_context_character(&db);
        db.conn.execute("ALTER TABLE contexts ADD COLUMN reviewer_id BLOB", []).unwrap();
        let root = make_context_row(Some("amy"));
        insert_context_with_doc(&db, &root, ws_id);
        set_stored_played_by(&db, root.context_id, Some(amy));
        set_stored_root_ctx(&db, amy, root.context_id);
        let mut seat = make_context_row(Some("seat"));
        seat.forked_from = Some(root.context_id);
        insert_context_with_doc(&db, &seat, ws_id);
        set_stored_played_by(&db, seat.context_id, Some(banto));
        let mut judge_seat = make_context_row(Some("judge-seat"));
        judge_seat.forked_from = Some(root.context_id);
        insert_context_with_doc(&db, &judge_seat, ws_id);
        set_stored_played_by(&db, judge_seat.context_id, Some(judge));
        let mut lane = make_context_row(Some("lane"));
        lane.forked_from = Some(seat.context_id);
        insert_context_with_doc(&db, &lane, ws_id);
        set_stored_played_by(&db, lane.context_id, Some(coder));
        db.conn.execute(
            "UPDATE contexts SET reviewer_id = ?1 WHERE context_id = ?2",
            params![blob_param(judge.as_bytes()), blob_param(lane.context_id.as_bytes())],
        ).unwrap();
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        assert_eq!(db.senpai_of(lane.context_id).unwrap(), Some(seat.context_id));
        assert!(db.senpai_migration_report().unwrap().is_empty());
        let has_reviewer: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('contexts') WHERE name = 'reviewer_id')", [], |r| r.get(0),
        ).unwrap();
        assert!(!has_reviewer);
    }

    /// Same idempotency contract as `last_activity_at_migration_...` above,
    /// for the `origin_host` column this change adds. A pre-migration row
    /// (simulated here by simply never calling `set_origin_host`) must still
    /// round-trip as `None` — "old rows without the field still render" is
    /// the same fact as "a fresh, never-set row reads back as None".
    #[test]
    fn origin_host_migration_is_idempotent_no_op_on_fresh_db() {
        let db = KernelDb::temporary().unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("origin-host-mig-check"));
        insert_context_with_doc(&db, &row, ws_id);
        let loaded = db.get_context(row.context_id).unwrap().unwrap();
        assert_eq!(
            loaded.origin_host, None,
            "fresh/pre-migration row: never set, must read back as None (\"-\" on display), \
             not error or default to some fabricated string"
        );
    }

    /// A real `ALTER TABLE` failure — not the expected/ignorable "duplicate
    /// column name" case — must propagate loudly, not vanish behind a
    /// swallowed `let _ = conn.execute(...)`. Drop a table one of the
    /// migrations targets so its ALTER fails for a genuine reason ("no such
    /// table"), and confirm `apply_additive_migrations` surfaces it as an
    /// `Err` instead of silently continuing.
    #[test]
    fn apply_additive_migrations_propagates_real_errors() {
        let db = KernelDb::temporary().unwrap();
        db.conn.execute_batch("DROP TABLE tracks").unwrap();
        let result = KernelDb::apply_additive_migrations(&db.conn);
        match result {
            Err(KernelDbError::Db(rusqlite::Error::SqliteFailure(_, Some(msg)))) => {
                assert!(
                    msg.contains("no such table"),
                    "expected a no-such-table failure, got: {msg}"
                );
            }
            other => panic!("a real ALTER TABLE failure must propagate: {other:?}"),
        }
    }

    /// `is_duplicate_column_error` is the guard that lets
    /// `apply_additive_migrations` re-run safely against an already-migrated
    /// DB — it must match SQLite's actual "duplicate column name" wording and
    /// nothing else (a different message must not be misclassified as the
    /// ignorable case and swallowed).
    #[test]
    fn duplicate_column_error_detection_is_specific() {
        let db = KernelDb::temporary().unwrap();
        let dup = db
            .conn
            .execute(
                "ALTER TABLE tracks ADD COLUMN deleted_at INTEGER",
                [],
            )
            .unwrap_err();
        assert!(is_duplicate_column_error(&dup), "expected duplicate-column: {dup}");

        let other = db
            .conn
            .execute("ALTER TABLE no_such_table ADD COLUMN x INTEGER", [])
            .unwrap_err();
        assert!(
            !is_duplicate_column_error(&other),
            "a no-such-table failure must not be misclassified as duplicate-column: {other}"
        );
    }

    /// Contexts created before the `house` capability existed keep the kj
    /// administration verbs: the migration grants `house` to every existing
    /// non-empty binding except the worker types', once.
    #[test]
    fn house_capability_migrates_onto_pre_existing_bindings_once() {
        use crate::mcp::{Capability, ContextToolBinding};
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let mut ids = std::collections::BTreeMap::new();
        for ty in ["default", "mcp", "director", "coder", "toolie", "musician", "empty", "unbound"] {
            let mut row = make_context_row(Some(ty));
            row.context_type = if matches!(ty, "empty" | "unbound") { "default" } else { ty }.to_string();
            insert_context_with_doc(&db, &row, ws_id);
            let mut b = ContextToolBinding::new();
            match ty {
                "empty" => {}
                "unbound" => {
                    ids.insert(ty, row.context_id);
                    continue;
                }
                _ => {
                    b.grant(Capability::Facade("shell".into()));
                    b.grant(Capability::Operator);
                }
            }
            db.upsert_context_binding(row.context_id, &b).unwrap();
            ids.insert(ty, row.context_id);
        }
        // The shape of a database from before this migration: no marker.
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::HOUSE_MIGRATION]).unwrap();
        fn holds(db: &KernelDb, id: ContextId) -> bool {
            db.get_context_binding(id).unwrap().is_some_and(|b| b.allows(&Capability::House))
        }
        assert!(!holds(&db, ids["default"]), "fixture: old binding has no house");

        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        for ty in ["default", "mcp", "director"] {
            assert!(holds(&db, ids[ty]), "{ty} binding should gain house");
        }
        for ty in ["coder", "toolie", "musician"] {
            assert!(!holds(&db, ids[ty]), "{ty} binding must not gain house");
        }
        // A context that grants nothing stays empty: house must not turn
        // "no usable loadout" into a usable one.
        assert!(db.get_context_binding(ids["empty"]).unwrap().unwrap().is_empty());
        assert!(db.get_context_binding(ids["unbound"]).unwrap().is_none());
        // The grant is additive.
        let kept = db.get_context_binding(ids["default"]).unwrap().unwrap();
        assert!(kept.allows(&Capability::Operator) && kept.allows(&Capability::Facade("shell".into())));

        // One-time: a seat that later drops house is not given it back.
        let mut narrowed = db.get_context_binding(ids["default"]).unwrap().unwrap();
        narrowed.revoke_cap(&Capability::House);
        db.upsert_context_binding(ids["default"], &narrowed).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        assert!(!holds(&db, ids["default"]), "the migration must run once");
        assert!(holds(&db, ids["mcp"]), "a second run changes nothing else");
    }

    /// `context_usage.cache_ttl_secs` must land on a DB created before the
    /// column existed, via `apply_additive_migrations`'s guarded `ALTER
    /// TABLE ... ADD COLUMN`, not just on a fresh `SCHEMA`. Simulates a
    /// pre-existing DB by dropping the column back off a freshly-opened
    /// (already-migrated) table, then re-running the migration and
    /// confirming a full `set_context_usage`/`get_context_usage` round trip
    /// carries the value through.
    #[test]
    fn cache_ttl_secs_migrates_onto_a_pre_existing_context_usage_table() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute_batch("ALTER TABLE context_usage DROP COLUMN cache_ttl_secs")
            .unwrap();

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-ttl-migration"));
        insert_context_with_doc(&db, &ctx, ws_id);
        db.set_context_usage(&ContextUsageRow {
            context_id: ctx.context_id,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 50,
            cache_write_tokens: 10,
            reasoning_tokens: 0,
            cache_ttl_secs: 3600,
            updated_at: 1,
        })
        .unwrap();

        let loaded = db.get_context_usage(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.cache_ttl_secs, 3600);
    }

    /// Re-running the migration against an already-migrated DB (the normal
    /// case on every `open()`) must be a no-op, not clobber an existing
    /// non-default value with the column's `DEFAULT 0` — the ADD COLUMN
    /// guard (`is_duplicate_column_error`) exists exactly for this.
    #[test]
    fn cache_ttl_secs_migration_is_idempotent_and_preserves_existing_value() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-ttl-idempotent"));
        insert_context_with_doc(&db, &ctx, ws_id);
        db.set_context_usage(&ContextUsageRow {
            context_id: ctx.context_id,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 50,
            cache_write_tokens: 10,
            reasoning_tokens: 0,
            cache_ttl_secs: 300,
            updated_at: 1,
        })
        .unwrap();

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let loaded = db.get_context_usage(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.cache_ttl_secs, 300);
    }

    // ── Context model pins + Track D preset cast narrowing ──────────────

    /// `migrate_preset_cast_narrowing` against a pre-Track-D `presets` table
    /// (still carrying `provider`/`model`): the legacy columns are dropped
    /// and a preset that had a provider/model loses it; no cast column is
    /// added.
    /// The pre-character-slot shape: the cast tables and `characters.cast_id`,
    /// which a fresh database no longer creates.
    fn with_cast_tables(db: &KernelDb) {
        KernelDb::ensure_legacy_cast_tables(&db.conn).unwrap();
        db.conn.execute("ALTER TABLE characters ADD COLUMN cast_id BLOB REFERENCES casts(cast_id) ON DELETE SET NULL", []).unwrap();
    }

    fn legacy_cast(db: &KernelDb, label: &str, slots: &[(&str, &str, Option<&str>)], backend: BackendId) -> Vec<u8> {
        let cast_id = CastId::new().as_bytes().to_vec();
        db.conn.execute(
            "INSERT INTO casts (cast_id, label, created_by) VALUES (?1, ?2, ?3)",
            params![cast_id, label, blob_param(PrincipalId::system().as_bytes())],
        ).unwrap();
        for (role, model, loadout) in slots {
            db.conn.execute(
                "INSERT INTO cast_slots (cast_id, role, backend_id, model, max_tokens, temperature, loadout)
                 VALUES (?1, ?2, ?3, ?4, 4096, 0.5, ?5)",
                params![cast_id, role, blob_param(backend.as_bytes()), model, loadout],
            ).unwrap();
        }
        cast_id
    }

    /// A database still on casts is copied aside before they move, and the
    /// copy holds the casts. A database already on character slots makes no
    /// copy.
    #[test]
    fn a_database_on_casts_is_copied_aside_before_it_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        drop(KernelDb::open(&path).unwrap());
        assert!(!KernelDb::pre_character_slots_backup_path(&path).exists(), "a database on the new shape makes no backup");
        {
            let conn = Connection::open(&path).unwrap();
            KernelDb::ensure_legacy_cast_tables(&conn).unwrap();
            conn.execute("ALTER TABLE characters ADD COLUMN cast_id BLOB", []).unwrap();
            conn.execute(
                "INSERT INTO casts (cast_id, label, created_by) VALUES (?1, 'house', ?2)",
                params![CastId::new().as_bytes().to_vec(), blob_param(PrincipalId::system().as_bytes())],
            ).unwrap();
        }

        let db = KernelDb::open(&path).unwrap();

        assert!(!KernelDb::has_table(&db.conn, "casts").unwrap(), "the casts were dropped");
        let backup = KernelDb::pre_character_slots_backup_path(&path);
        let copy = Connection::open(&backup).unwrap();
        let labels: Vec<String> = copy.prepare("SELECT label FROM casts").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<SqliteResult<_>>().unwrap();
        assert_eq!(labels, vec!["house"], "the copy holds what the migration dropped");
    }

    /// A copy that cannot be made refuses with the reason, and a database
    /// that has no casts is not copied at all.
    #[test]
    fn a_failed_backup_refuses_naming_the_path() {
        let db = KernelDb::temporary().unwrap();
        KernelDb::ensure_legacy_cast_tables(&db.conn).unwrap();
        let unwritable = Path::new("/nonexistent-kernel-db-directory/kernel.db");
        let error = KernelDb::back_up_before_character_slots(&db.conn, unwritable).unwrap_err().to_string();
        assert!(error.contains("kernel.db.pre-character-slots") && error.contains("could not be made"), "{error}");

        KernelDb::migrate_character_slots(&db.conn).unwrap();
        KernelDb::back_up_before_character_slots(&db.conn, unwritable)
            .expect("a database with no casts needs no copy, so an unwritable path is not consulted");
    }

    /// Each character gets a copy of the slots of the cast it names; a
    /// character with no cast gets none; the casts, the cast column and the
    /// marker follow, and a second run changes nothing.
    #[test]
    fn character_slots_migration_copies_each_characters_cast_and_drops_the_casts() {
        let db = KernelDb::temporary().unwrap();
        with_cast_tables(&db);
        let backend = db.upsert_backend(&BackendRow {
            backend_id: BackendId::new(), name: "mock".into(), kind: "mock".into(), base_url: None, api_key_env: None,
            api_key_file: None, key_optional: true, request_timeout_secs: None, idle_timeout_secs: None,
            max_concurrent: None, created_at: 0, created_by: PrincipalId::system(),
        }).unwrap().backend_id;
        let [a, b, c] = seed_characters(&db, &["has-house", "no-cast", "also-house"])[..] else { unreachable!() };
        let house = legacy_cast(&db, "house", &[("coder", "m-coder", Some("lo")), ("distill", "m-distill", None)], backend);
        legacy_cast(&db, "orphan", &[("coder", "m-orphan", None)], backend);
        for who in [a, c] {
            db.conn.execute("UPDATE characters SET cast_id = ?1 WHERE principal_id = ?2", params![house, blob_param(who.as_bytes())]).unwrap();
        }

        KernelDb::migrate_character_slots(&db.conn).unwrap();

        for who in [a, c] {
            let slots = db.list_character_slots(who).unwrap();
            let roles: Vec<(&str, &str)> = slots.iter().map(|s| (s.role.as_str(), s.model.as_str())).collect();
            assert_eq!(roles, vec![("coder", "m-coder"), ("distill", "m-distill")]);
            assert_eq!((slots[0].max_tokens, slots[0].temperature, slots[0].backend_id), (Some(4096), Some(0.5), backend));
        }
        assert!(db.list_character_slots(b).unwrap().is_empty(), "a character with no cast gets no slot");
        assert!(!KernelDb::has_table(&db.conn, "casts").unwrap() && !KernelDb::has_table(&db.conn, "cast_slots").unwrap());
        assert!(!KernelDb::has_column(&db.conn, "characters", "cast_id").unwrap());

        KernelDb::migrate_character_slots(&db.conn).unwrap();
        assert_eq!(db.list_character_slots(a).unwrap().len(), 2, "a second run changes nothing");
    }

    /// A database that has not been through the context character migration
    /// still has its casts to read, so the move refuses and writes nothing.
    #[test]
    fn character_slots_migration_refuses_before_the_context_character_migration() {
        let db = KernelDb::temporary().unwrap();
        with_cast_tables(&db);
        db.conn.execute("ALTER TABLE characters ADD COLUMN default_cast_id BLOB", []).unwrap();
        let error = KernelDb::migrate_character_slots(&db.conn).unwrap_err().to_string();
        assert!(error.contains("context character migration"), "{error}");
        assert!(KernelDb::has_table(&db.conn, "casts").unwrap(), "nothing was dropped");
    }

    /// A database on the new shape has no cast tables and does nothing.
    #[test]
    fn character_slots_migration_is_a_no_op_on_a_fresh_database() {
        let db = KernelDb::temporary().unwrap();
        assert!(!KernelDb::has_table(&db.conn, "casts").unwrap(), "a fresh database creates no casts");
        KernelDb::migrate_character_slots(&db.conn).unwrap();
        assert!(KernelDb::has_table(&db.conn, "character_slots").unwrap());
    }

    #[test]
    fn preset_cast_narrowing_drops_the_legacy_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE casts (
                cast_id BLOB NOT NULL PRIMARY KEY,
                label TEXT NOT NULL UNIQUE
             );
             CREATE TABLE presets (
                preset_id BLOB NOT NULL PRIMARY KEY,
                label TEXT NOT NULL UNIQUE,
                description TEXT,
                provider TEXT,
                model TEXT,
                system_prompt TEXT,
                consent_mode TEXT NOT NULL DEFAULT 'collaborative',
                created_at INTEGER NOT NULL,
                created_by BLOB NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO presets
                (preset_id, label, description, provider, model, system_prompt,
                 consent_mode, created_at, created_by)
             VALUES (?1, 'legacy-research', NULL, 'anthropic', 'claude-opus-4-6', NULL,
                     'autonomous', 0, ?2)",
            params![vec![1u8; 16], vec![2u8; 16]],
        )
        .unwrap();

        KernelDb::migrate_preset_cast_narrowing(&conn).unwrap();

        let columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(presets)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<SqliteResult<_>>()
                .unwrap()
        };
        assert!(!columns.contains(&"provider".to_string()), "provider column dropped");
        assert!(!columns.contains(&"model".to_string()), "model column dropped");
        assert!(!columns.contains(&"cast_id".to_string()), "no cast column is added");

        // Idempotent re-run: already-migrated columns, no-op.
        KernelDb::migrate_preset_cast_narrowing(&conn).unwrap();
    }

    /// A brand-new DB (current `SCHEMA`, no legacy columns ever existed) is
    /// a clean no-op — the guard must not error just because there was
    /// nothing to narrow.
    #[test]
    fn preset_cast_narrowing_is_a_no_op_on_a_fresh_db() {
        let db = KernelDb::temporary().unwrap();
        // temporary() already ran it once as part of open; running again
        // directly must still be a clean no-op.
        KernelDb::migrate_preset_cast_narrowing(&db.conn).unwrap();
    }

    // ── approval-ledger migration ──────────────────────────────────────

    /// `KernelDb::open` must migrate the approval-ledger schema into the
    /// kernel DB, and a re-open of an already-migrated file must be a no-op
    /// (the ledger's `CREATE TABLE IF NOT EXISTS` is idempotent). Guards the
    /// boot path the `kj cc send` gate depends on: a live kernel that
    /// restarts onto its existing `kernel.db` must find the ledger tables
    /// every time.
    #[test]
    fn open_migrates_the_ledger_schema_and_reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ledger.db");

        {
            let db = KernelDb::open(&db_path).unwrap();
            let n: i64 = db
                .conn_for_ledger()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approvals'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "open() must create the ledger's approvals table");
        }
        // Drop closed the connection; re-open the same file.
        {
            let db = KernelDb::open(&db_path).unwrap();
            let n: i64 = db
                .conn_for_ledger()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approvals'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "re-open must be an idempotent no-op");
        }
    }

    // ── synchronous pragma ────────────────────────────────────────────

    /// `init_connection` must set `synchronous = NORMAL` (1), not SQLite's
    /// default `FULL` (2). Under WAL, NORMAL fsyncs at checkpoint instead of
    /// on every commit, and a streaming turn commits once per delta.
    #[test]
    fn init_connection_sets_synchronous_normal() {
        let db = KernelDb::temporary().unwrap();
        let mode: i64 = db
            .conn_for_ledger()
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 1, "synchronous should be NORMAL (1), got {mode}");
    }

    // ── journal size limit ───────────────────────────────────────────────

    /// `init_connection` must cap the WAL's high-water mark with
    /// `journal_size_limit`, or SQLite's default of -1 (no limit) lets a busy
    /// kernel's `-wal` file grow without bound and never shrink back down,
    /// since a checkpoint only truncates a WAL larger than this limit.
    #[test]
    fn init_connection_sets_journal_size_limit() {
        let db = KernelDb::temporary().unwrap();
        let limit: i64 = db
            .conn_for_ledger()
            .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            limit, 67_108_864,
            "journal_size_limit should be 67108864 (64 MiB), got {limit}"
        );
    }

    // ── WAL checkpoint ──────────────────────────────────────────────────

    /// `checkpoint()` (TRUNCATE) must flush committed frames into the main
    /// file and shrink the `-wal` file back to zero. Guards the proactive
    /// checkpoint that compaction relies on so a bare-file read of the main
    /// `.db` stops lagging committed history (2026-06-11 forensics). On-disk
    /// db required — a `:memory:` connection has no WAL file.
    #[test]
    fn checkpoint_truncates_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ckpt.db");
        let wal_path = dir.path().join("ckpt.db-wal");

        let db = KernelDb::open(&db_path).unwrap();
        let ws_id = db
            .get_or_create_default_workspace(PrincipalId::system())
            .unwrap();
        let doc = ContextId::new();
        db.insert_document(&DocumentRow {
            document_id: doc,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();

        // 200 × 4 KiB ops ≈ 800 KiB — grows the WAL but stays under SQLite's
        // 1000-page (~4 MiB) auto-checkpoint, so the WAL is non-empty and
        // un-truncated when we reach the manual checkpoint below.
        let payload = vec![0xABu8; 4096];
        for seq in 1..=200 {
            db.append_op(doc, seq, &payload).unwrap();
        }

        let wal_before = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert!(
            wal_before > 0,
            "WAL should have grown after 200 appends, got {wal_before} bytes",
        );

        let (busy, _log, _checkpointed) = db.checkpoint().unwrap();
        assert_eq!(busy, 0, "single-connection checkpoint must not be busy");

        // The behavioral proof: TRUNCATE shrinks the -wal file back to zero,
        // so a bare-file read of the main .db now reflects all 200 ops. (The
        // log/checkpointed frame counts are version-dependent for TRUNCATE and
        // not asserted.)
        let wal_after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            wal_after, 0,
            "TRUNCATE checkpoint should zero the -wal file, got {wal_after} bytes",
        );
    }

    // ── VACUUM INTO backup ──────────────────────────────────────────────

    /// `vacuum_into` must produce a real, non-empty file — the behavioral
    /// guard against a silent no-op (e.g. a typo'd pragma or a swallowed
    /// error that still returns `Ok`). A bug that made `vacuum_into` write
    /// nothing would still pass a test that only checked `is_ok()`; this one
    /// also asserts the file exists and has SQLite's magic header size.
    #[test]
    fn vacuum_into_produces_nonempty_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("backup.db");

        let db = KernelDb::open(&db_path).unwrap();
        db.get_or_create_default_workspace(PrincipalId::system())
            .unwrap();

        assert!(!backup_path.exists(), "precondition: backup must not exist yet");
        db.vacuum_into(&backup_path).unwrap();

        let meta = std::fs::metadata(&backup_path)
            .expect("vacuum_into must create the backup file");
        assert!(
            meta.len() >= 100,
            "backup file too small to be a real SQLite db ({} bytes) — vacuum_into may have \
             written nothing",
            meta.len()
        );
    }

    /// The backup must be a fully valid, queryable SQLite database containing
    /// the same rows as the source — not just a non-empty file. Opens the
    /// backup as an independent `KernelDb` and checks it has the same
    /// singleton kernel id and the workspace/document rows written before
    /// the backup was taken.
    #[test]
    fn vacuum_into_backup_is_valid_and_contains_expected_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("backup.db");

        let db = KernelDb::open(&db_path).unwrap();
        let ws_id = db
            .get_or_create_default_workspace(PrincipalId::system())
            .unwrap();
        let doc = ContextId::new();
        db.insert_document(&DocumentRow {
            document_id: doc,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();
        let source_kernel_id = db.kernel_id().unwrap();

        db.vacuum_into(&backup_path).unwrap();

        // Open the backup as an independent connection — proves it's a
        // structurally valid SQLite file with the full schema, not a
        // truncated or corrupt copy.
        let backup = KernelDb::open(&backup_path).unwrap();
        assert_eq!(
            backup.kernel_id().unwrap(),
            source_kernel_id,
            "backup must carry the same singleton kernel identity"
        );
        let restored_doc = backup
            .get_document(doc)
            .expect("query backup db")
            .expect("document present in backup");
        assert_eq!(restored_doc.doc_kind, DocKind::Conversation);
    }

    /// SQLite refuses to `VACUUM INTO` an existing file. The error must
    /// surface cleanly (not panic, not silently succeed by overwriting) —
    /// `kj db backup` relies on this to avoid clobbering a same-named backup.
    #[test]
    fn vacuum_into_target_exists_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("backup.db");
        std::fs::write(&backup_path, b"not a database, just occupying the name").unwrap();

        let db = KernelDb::open(&db_path).unwrap();
        let result = db.vacuum_into(&backup_path);

        assert!(
            result.is_err(),
            "vacuum_into must error when the target file already exists"
        );
        // The pre-existing (bogus) file content must be left untouched —
        // vacuum_into must not have deleted-then-recreated it.
        let contents = std::fs::read(&backup_path).unwrap();
        assert_eq!(contents, b"not a database, just occupying the name");
    }

    /// A path with a `'` — the SQL string-literal delimiter — must not be
    /// able to break out of the `VACUUM INTO` statement. Guards the bound-
    /// parameter choice (`?1`) over string interpolation.
    #[test]
    fn vacuum_into_path_with_quote_is_bound_not_interpolated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("source.db");
        let backup_path = dir.path().join("back'up.db");

        let db = KernelDb::open(&db_path).unwrap();
        db.vacuum_into(&backup_path).unwrap();

        assert!(
            backup_path.exists(),
            "a bound parameter should handle a quote in the filename literally"
        );
    }

    // ── 2. Context lifecycle ────────────────────────────────────────────

    // ── 3. Label validation: colon ──────────────────────────────────────

    #[test]
    fn label_validation_colon() {
        let db = KernelDb::temporary().unwrap();
        let row = make_context_row(Some("my:label"));

        let err = db.insert_context(&row).unwrap_err();
        assert!(matches!(err, KernelDbError::InvalidLabel(_)));
    }

    // ── 4. Label uniqueness ────────────────────────────────────────────

    // ── 5. Fork lineage 3 deep ─────────────────────────────────────────

    // ── 8. Drift edge allows dupes ─────────────────────────────────────

    // ── 10. Preset lifecycle ───────────────────────────────────────────

    // ── 11. Workspace lifecycle ────────────────────────────────────────

    // ── 12. Workspace soft delete ──────────────────────────────────────

    // ── 13. Drift push creates edge ────────────────────────────────────

    // ── 14. Context DAG recursive ──────────────────────────────────────

    // ── Dropping the structural edge rows ──────────────────────────────

    /// Give `context_edges` the shape it had while it carried structural
    /// rows: the `kind` column and its partial unique index.
    fn restore_edge_kind_column(db: &KernelDb) {
        db.conn
            .execute_batch(
                "ALTER TABLE context_edges ADD COLUMN kind TEXT NOT NULL DEFAULT 'structural';
                 CREATE UNIQUE INDEX idx_edges_structural_unique
                     ON context_edges(source_id, target_id) WHERE kind = 'structural';",
            )
            .unwrap();
    }

    fn insert_legacy_edge(db: &KernelDb, source: ContextId, target: ContextId, kind: &str) {
        db.conn
            .execute(
                "INSERT INTO context_edges (edge_id, source_id, target_id, kind, created_at)
                 VALUES (?1, ?2, ?3, ?4, 0)",
                params![
                    uuid::Uuid::now_v7().as_bytes().as_slice(),
                    blob_param(source.as_bytes()),
                    blob_param(target.as_bytes()),
                    kind
                ],
            )
            .unwrap();
    }

    fn edge_count(db: &KernelDb) -> i64 {
        db.conn
            .query_row("SELECT COUNT(*) FROM context_edges", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn dropping_structural_edges_keeps_drift_and_forked_from() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let parent = make_context_row(Some("parent"));
        let mut child = make_context_row(Some("child"));
        child.forked_from = Some(parent.context_id);
        insert_context_with_doc(&db, &parent, ws_id);
        insert_context_with_doc(&db, &child, ws_id);
        restore_edge_kind_column(&db);
        insert_legacy_edge(&db, parent.context_id, child.context_id, "structural");
        insert_legacy_edge(&db, child.context_id, parent.context_id, "drift");

        KernelDb::drop_structural_edges(&db.conn).unwrap();

        assert!(!KernelDb::has_column(&db.conn, "context_edges", "kind").unwrap());
        assert_eq!(edge_count(&db), 1, "only the drift row remains");
        let edges = db.edges_from(child.context_id).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_id, parent.context_id);
        assert_eq!(
            db.get_context(child.context_id).unwrap().unwrap().forked_from,
            Some(parent.context_id)
        );
        // A second run finds the column gone and changes nothing.
        KernelDb::drop_structural_edges(&db.conn).unwrap();
        assert_eq!(edge_count(&db), 1);
    }

    #[test]
    fn dropping_structural_edges_refuses_a_parent_that_is_not_forked_from() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let edge_parent = make_context_row(Some("edge-parent"));
        let fork_parent = make_context_row(Some("fork-parent"));
        let mut child = make_context_row(Some("child"));
        child.forked_from = Some(fork_parent.context_id);
        let mut orphan = make_context_row(Some("orphan"));
        orphan.forked_from = None;
        for ctx in [&edge_parent, &fork_parent, &child, &orphan] {
            insert_context_with_doc(&db, ctx, ws_id);
        }
        restore_edge_kind_column(&db);
        insert_legacy_edge(&db, edge_parent.context_id, child.context_id, "structural");
        insert_legacy_edge(&db, edge_parent.context_id, orphan.context_id, "structural");

        let err = KernelDb::drop_structural_edges(&db.conn).unwrap_err().to_string();

        assert!(err.contains(&child.context_id.short()), "names the child: {err}");
        assert!(err.contains(&orphan.context_id.short()), "names the orphan: {err}");
        assert!(err.contains("forked_from"), "{err}");
        assert!(KernelDb::has_column(&db.conn, "context_edges", "kind").unwrap(), "nothing was dropped");
        assert_eq!(edge_count(&db), 2, "nothing was deleted");
    }

    #[test]
    fn dropping_structural_edges_refuses_two_structural_parents() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let one = make_context_row(Some("one"));
        let two = make_context_row(Some("two"));
        let mut child = make_context_row(Some("child"));
        child.forked_from = Some(one.context_id);
        for ctx in [&one, &two, &child] {
            insert_context_with_doc(&db, ctx, ws_id);
        }
        restore_edge_kind_column(&db);
        insert_legacy_edge(&db, one.context_id, child.context_id, "structural");
        insert_legacy_edge(&db, two.context_id, child.context_id, "structural");

        let err = KernelDb::drop_structural_edges(&db.conn).unwrap_err().to_string();

        assert!(err.contains("2 structural parents"), "{err}");
        assert!(err.contains(&child.context_id.short()), "{err}");
        assert_eq!(edge_count(&db), 2, "nothing was deleted");
    }

    #[test]
    fn dropping_structural_edges_refuses_an_unknown_kind() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let a = make_context_row(Some("a"));
        let b = make_context_row(Some("b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);
        restore_edge_kind_column(&db);
        insert_legacy_edge(&db, a.context_id, b.context_id, "mystery");

        let err = KernelDb::drop_structural_edges(&db.conn).unwrap_err().to_string();

        assert!(err.contains("unknown kind 'mystery'"), "{err}");
        assert_eq!(edge_count(&db), 1);
    }

    // ── 15. Tag resolution (label + prefix) ────────────────────────────

    // ── 16. Resolve context basic ──────────────────────────────────────

    // ── 17. Null labels coexist ────────────────────────────────────────

    // ── 18. Archive excludes from active + fork_children ─────────

    // ── 19. Context edge cascade ───────────────────────────────────────

    // ── 20. Context row to_context conversion ──────────────────────────

    // ── 21. Roundtrip: create context, read back, verify all 15 fields ──

    // ── 22. FK violation produces Validation, not LabelConflict ──────────

    #[test]
    fn fk_violation_is_validation_error() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        // Reference a workspace_id that doesn't exist on the context
        let ctx_id = ContextId::new();
        // Insert document with valid workspace first
        db.insert_document(&DocumentRow {
            document_id: ctx_id,
                        workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::new(),
        })
        .unwrap();

        let row = ContextRow {
            context_id: ctx_id,
            label: Some("fk-test".into()),
            system_prompt: None,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: now_millis(),
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: Some(WorkspaceId::new()), // doesn't exist
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            origin_host: None,
            played_by: None,
        };
        let err = db.insert_context(&row).unwrap_err();
        assert!(
            matches!(err, KernelDbError::Validation(_)),
            "expected Validation for FK violation, got: {err}"
        );
    }

    // ── 22b. Characters CRUD ──────────────────────────────────────────

    // ── 22c. the `root` flag ────────────────────────────────────────────

    /// A `characters` table on the shape that shipped `accountable_to`
    /// comes back with that column dropped and `root` added, and the rows
    /// that were in it survive both moves.
    #[test]
    fn root_migration_adds_the_flag_and_drops_accountable_to() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute_batch(
                "DROP TABLE characters;
                 CREATE TABLE characters (
                     principal_id   BLOB NOT NULL PRIMARY KEY,
                     name           TEXT NOT NULL UNIQUE,
                     created_at     INTEGER NOT NULL,
                     retired_at     INTEGER,
                     handoff_ctx    BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
                     accountable_to BLOB REFERENCES characters(principal_id) ON DELETE RESTRICT
                 );",
            )
            .unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO characters (principal_id, name, created_at, retired_at, handoff_ctx, accountable_to) \
                 VALUES (?1, ?2, ?3, NULL, NULL, NULL)",
                params![blob_param(principal.as_bytes()), "pre-existing", 1000],
            )
            .unwrap();
        let has = |column: &str| -> bool {
            db.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('characters') WHERE name = ?1)",
                params![column], |row| row.get(0),
            ).unwrap()
        };
        assert!(has("accountable_to"), "test setup must start on the old shape");
        assert!(!has("root"), "test setup must start without the flag");

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        assert!(!has("accountable_to"), "the guarded DROP must remove accountable_to");
        assert!(has("root"), "the guarded ALTER must add root");
        let row = db.get_character(principal).unwrap().unwrap();
        assert_eq!(row.name, "pre-existing", "an existing row must survive the migration");
        assert!(!row.root, "a pre-migration row is not a root");
        assert!(has("context_type"), "the guarded ALTER must add context_type");
        assert_eq!(row.context_type, None, "a pre-migration row has no context type");

        // A second pass over the already-migrated database is a no-op.
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        assert!(!has("accountable_to"));
        assert!(has("root"));
    }

    // ── 22e. the senpai migration ───────────────────────────────────────

    /// Give the database the routing schema the pre-senpai kernel had:
    /// `contexts.reviewer_id`, `contexts.director_id` and the delegation
    /// tables. Idempotent.
    fn ensure_old_routing_schema(db: &KernelDb) {
        into_pre_context_character(db);
        for sql in ["ALTER TABLE contexts ADD COLUMN reviewer_id BLOB", "ALTER TABLE contexts ADD COLUMN director_id BLOB"] {
            match db.conn.execute(sql, []) {
                Ok(_) => {}
                Err(e) if is_duplicate_column_error(&e) => {}
                Err(e) => panic!("{sql}: {e}"),
            }
        }
        db.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS approval_reviewer_delegations (
                director_id BLOB NOT NULL PRIMARY KEY,
                reviewer_id BLOB NOT NULL,
                granted_by BLOB NOT NULL,
                granted_at INTEGER NOT NULL,
                revoked_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS approval_reviewer_delegation_events (
                event_id BLOB NOT NULL PRIMARY KEY,
                director_id BLOB NOT NULL,
                reviewer_id BLOB,
                actor_id BLOB NOT NULL,
                operation TEXT NOT NULL CHECK (operation IN ('grant', 'revoke')),
                occurred_at INTEGER NOT NULL
            );",
        ).unwrap();
    }

    /// An active delegation row as the pre-senpai kernel wrote it.
    fn old_delegation(db: &KernelDb, director: PrincipalId, reviewer: PrincipalId, granted_by: PrincipalId) {
        ensure_old_routing_schema(db);
        db.conn.execute(
            "INSERT INTO approval_reviewer_delegations (director_id, reviewer_id, granted_by, granted_at, revoked_at)
             VALUES (?1, ?2, ?3, 1, NULL)",
            params![blob_param(director.as_bytes()), blob_param(reviewer.as_bytes()), blob_param(granted_by.as_bytes())],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO approval_reviewer_delegation_events (event_id, director_id, reviewer_id, actor_id, operation, occurred_at)
             VALUES (?1, ?2, ?3, ?4, 'grant', 1)",
            params![uuid::Uuid::now_v7().as_bytes(), blob_param(director.as_bytes()), blob_param(reviewer.as_bytes()), blob_param(granted_by.as_bytes())],
        ).unwrap();
    }

    fn has_column(db: &KernelDb, table: &str, column: &str) -> bool {
        db.conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"), [column], |r| r.get(0),
        ).unwrap()
    }

    fn has_table(db: &KernelDb, table: &str) -> bool {
        db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [table], |r| r.get(0),
        ).unwrap()
    }

    /// A context as the pre-senpai kernel wrote it: routed by `forked_from`,
    /// `played_by`, `director_id` and `reviewer_id`, with no senpai.
    fn old_context(
        db: &KernelDb,
        ws_id: WorkspaceId,
        label: &str,
        forked_from: Option<&ContextRow>,
        played_by: Option<PrincipalId>,
        created_at: i64,
    ) -> ContextRow {
        ensure_old_routing_schema(db);
        let mut row = make_context_row(Some(label));
        row.forked_from = forked_from.map(|p| p.context_id);
        row.created_at = created_at;
        insert_context_with_doc(db, &row, ws_id);
        set_stored_played_by(db, row.context_id, played_by);
        row.played_by = played_by;
        row
    }

    fn set_old_routing(db: &KernelDb, ctx: &ContextRow, director: Option<PrincipalId>, reviewer: Option<PrincipalId>) {
        db.conn.execute(
            "UPDATE contexts SET director_id = ?1, reviewer_id = ?2 WHERE context_id = ?3",
            params![
                director.as_ref().map(|p| p.as_bytes().to_vec()),
                reviewer.as_ref().map(|p| p.as_bytes().to_vec()),
                blob_param(ctx.context_id.as_bytes()),
            ],
        ).unwrap();
    }

    /// The open steps from the senpai mapping through the routing drop.
    fn senpai_open_steps(conn: &Connection) -> KernelDbResult<()> {
        KernelDb::migrate_senpai_edges(conn)?;
        KernelDb::drop_old_routing(conn)
    }

    /// Run the senpai migration the way an upgrading kernel does: the marker is absent.
    fn run_senpai_migration(db: &KernelDb) -> KernelDbResult<()> {
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();
        KernelDb::migrate_senpai_edges(&db.conn)
    }

    /// Write the stored `senpai_ctx` of a context that predates instances.
    fn set_stored_senpai(db: &KernelDb, ctx: &ContextRow, senpai: &ContextRow) {
        db.conn.execute(
            "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(senpai.context_id.as_bytes()), blob_param(ctx.context_id.as_bytes())],
        ).unwrap();
    }

    fn senpai_of(db: &KernelDb, ctx: &ContextRow) -> Option<ContextId> {
        db.conn.query_row(
            "SELECT senpai_ctx FROM contexts WHERE context_id = ?1",
            [blob_param(ctx.context_id.as_bytes())], |r| read_opt_context_id(r, 0),
        ).unwrap()
    }

    fn report_of(db: &KernelDb) -> Vec<(ContextId, Option<ContextId>, String)> {
        db.senpai_migration_report().unwrap()
    }

    /// The nearest-ancestor case maps exactly: each context's senpai is the
    /// nearest context above it whose holder the old resolver named, and
    /// nothing is reported. A self-held fork skips its own seat, a
    /// holderless parent is walked past, and a root console stays a top.
    #[test]
    fn senpai_migration_maps_the_nearest_ancestor_case_exactly() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();

        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
        let self_fork = old_context(&db, ws_id, "banto-fork", Some(&seat), Some(banto), 4);
        let holderless = old_context(&db, ws_id, "holderless", Some(&seat), None, 5);
        let under_holderless = old_context(&db, ws_id, "under-holderless", Some(&holderless), Some(coder), 6);
        let client = old_context(&db, ws_id, "client", Some(&root), None, 7);
        let scratch = old_context(&db, ws_id, "scratch", None, None, 8);
        let [gone] = seed_characters(&db, &["gone"])[..] else { unreachable!() };
        let retired_seat = old_context(&db, ws_id, "retired-seat", Some(&root), Some(gone), 9);
        let under_retired = old_context(&db, ws_id, "under-retired", Some(&retired_seat), Some(coder), 10);
        assert!(db.retire_character(gone, 1).unwrap());

        run_senpai_migration(&db).unwrap();
        run_context_character_migration(&db).unwrap();

        assert_eq!(senpai_of(&db, &root), None, "a root console is a top");
        assert_eq!(senpai_of(&db, &seat), Some(root.context_id));
        assert_eq!(senpai_of(&db, &lane), Some(seat.context_id), "the lane's reviewer was banto, who plays the seat");
        assert_eq!(senpai_of(&db, &self_fork), Some(root.context_id), "banto cannot review itself: the nearest holder that is not banto is amy's console");
        assert_eq!(senpai_of(&db, &holderless), Some(seat.context_id));
        assert_eq!(senpai_of(&db, &under_holderless), Some(seat.context_id), "walked past the parent nobody holds, as the old walk did");
        assert_eq!(senpai_of(&db, &client), Some(root.context_id), "a context nobody performs answers to the nearest holder above it");
        assert_eq!(senpai_of(&db, &scratch), None, "a parentless system context is a top");
        assert_eq!(senpai_of(&db, &under_retired), Some(retired_seat.context_id), "the old walk stopped at the retired holder and refused by name");
        let error = db.route_review(ReviewQuery::ask(under_retired.context_id, coder)).unwrap_err().to_string();
        assert!(error.contains("gone") && error.contains("retired"), "the new resolver refuses the same way: {error}");
        assert_eq!(report_of(&db), vec![], "every one of these was the nearest-ancestor case");
        for ctx in [&root, &seat, &lane, &self_fork, &under_holderless, &retired_seat] {
            assert_eq!(db.lineage_root(ctx.context_id).unwrap(), amy);
        }
        assert_eq!(db.context_character(client.context_id).unwrap(), None, "a context nobody performs gets no context character");
        let listed: Vec<ContextId> = db.context_character_migration_report().unwrap().into_iter()
            .filter(|n| n.kind == "unmapped").filter_map(|n| n.context_id).collect();
        assert!(listed.contains(&client.context_id), "and is listed because it had a senpai_ctx: {listed:?}");
    }

    /// The migration routes every context to the reviewer the old resolver
    /// would have named for its performer: same answer before and after.
    #[test]
    fn senpai_migration_preserves_the_reviewer_the_old_resolver_named() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
        let judged = old_context(&db, ws_id, "judged", Some(&seat), Some(coder), 4);
        set_old_routing(&db, &judged, Some(banto), Some(amy));
        let judge_seat = old_context(&db, ws_id, "judge-seat", Some(&root), Some(judge), 5);
        let _ = judge_seat;

        run_senpai_migration(&db).unwrap();
        run_context_character_migration(&db).unwrap();

        for (ctx, performer, reviewer) in [(&lane, coder, banto), (&seat, banto, amy), (&judged, coder, amy)] {
            assert_eq!(db.route_review(ReviewQuery::ask(ctx.context_id, performer)).unwrap().reviewer(), reviewer, "{:?}", ctx.label);
        }
        assert_eq!(senpai_of(&db, &judged), Some(root.context_id), "the override named amy, who holds an ancestor: nearest-ancestor case");
        assert_eq!(report_of(&db), vec![]);
    }

    /// An override or delegation that names a character outside the lineage
    /// maps to that character's root console when it is a root, else to its
    /// most recent live context, and the context is reported with the reason.
    #[test]
    fn senpai_migration_reports_what_it_cannot_map_to_an_ancestor() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, judge, bob, amy] = seed_characters(&db, &["coder", "banto", "judge", "bob", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        db.update_character_root(bob, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let bob_console = old_context(&db, ws_id, "bob", None, Some(bob), 2);
        set_stored_root_ctx(&db, bob, bob_console.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 3);
        let older = old_context(&db, ws_id, "judge-old", Some(&root), Some(judge), 4);
        let newer = old_context(&db, ws_id, "judge-new", Some(&root), Some(judge), 5);
        let archived = old_context(&db, ws_id, "judge-archived", Some(&root), Some(judge), 9);
        db.archive_context(archived.context_id).unwrap();
        let _ = older;

        let to_bob = old_context(&db, ws_id, "to-bob", Some(&seat), Some(coder), 6);
        set_old_routing(&db, &to_bob, Some(banto), Some(bob));
        let to_judge = old_context(&db, ws_id, "to-judge", Some(&seat), Some(coder), 7);
        set_old_routing(&db, &to_judge, Some(banto), Some(judge));
        let delegated = old_context(&db, ws_id, "delegated", Some(&seat), Some(coder), 8);
        set_old_routing(&db, &delegated, Some(banto), None);
        old_delegation(&db, banto, judge, amy);

        run_senpai_migration(&db).unwrap();

        assert_eq!(senpai_of(&db, &to_bob), Some(bob_console.context_id), "a root reviewer outside the lineage: its root console");
        assert_eq!(senpai_of(&db, &to_judge), Some(newer.context_id), "else its most recent live context, never an archived one");
        assert_eq!(senpai_of(&db, &delegated), Some(newer.context_id));
        assert_eq!(senpai_of(&db, &seat), Some(root.context_id), "the seat itself was the nearest-ancestor case");
        // The delegation also named the seat's other contexts of banto's direction.
        let report = report_of(&db);
        let reported: std::collections::BTreeMap<_, _> = report.iter().map(|(c, s, why)| (*c, (*s, why.clone()))).collect();
        assert_eq!(reported.len(), 3, "{report:?}");
        assert_eq!(reported[&to_bob.context_id].0, Some(bob_console.context_id));
        assert!(reported[&to_bob.context_id].1.contains("root console"), "{report:?}");
        assert_eq!(reported[&to_judge.context_id].0, Some(newer.context_id));
        assert!(reported[&to_judge.context_id].1.contains("most recent live context"), "{report:?}");
        assert!(reported[&delegated.context_id].1.contains("delegation"), "{report:?}");
        for why in reported.values().map(|(_, why)| why) {
            assert!(why.contains("senpai"), "the note says what to do: {why}");
        }
    }

    /// The migration runs once: a later call does not recompute senpai edges
    /// that were moved since.
    #[test]
    fn senpai_migration_runs_once() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [banto, coder] = seed_characters(&db, &["banto", "coder"])[..] else { unreachable!() };
        let top = old_context(&db, ws_id, "once-top", None, Some(banto), 1);
        let other = old_context(&db, ws_id, "once-other", None, Some(banto), 2);
        let kid = old_context(&db, ws_id, "once-kid", Some(&top), Some(coder), 3);
        run_senpai_migration(&db).unwrap();
        assert_eq!(senpai_of(&db, &kid), Some(top.context_id));

        set_stored_senpai(&db, &kid, &other);
        KernelDb::migrate_senpai_edges(&db.conn).unwrap();
        assert_eq!(senpai_of(&db, &kid), Some(other.context_id), "the marker keeps the moved edge");
    }

    /// On a database with the old routing (an override, a delegation, the
    /// two tables), opening maps every context to its senpai first, then
    /// drops `contexts.reviewer_id`, `contexts.director_id` and both
    /// delegation tables. Each context whose old route was not its nearest
    /// ancestor stays in the report, which keeps the old reviewer. A second
    /// open does nothing.
    #[test]
    fn opening_an_old_database_maps_the_senpai_then_drops_the_old_routing() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
        let judge_seat = old_context(&db, ws_id, "judge-seat", Some(&root), Some(judge), 4);
        let overridden = old_context(&db, ws_id, "overridden", Some(&seat), Some(coder), 5);
        set_old_routing(&db, &overridden, Some(banto), Some(judge));
        let delegated = old_context(&db, ws_id, "delegated", Some(&seat), Some(coder), 6);
        set_old_routing(&db, &delegated, Some(banto), None);
        old_delegation(&db, banto, judge, amy);
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();

        senpai_open_steps(&db.conn).unwrap();
        run_context_character_migration(&db).unwrap();

        assert_eq!(senpai_of(&db, &lane), Some(seat.context_id));
        assert_eq!(senpai_of(&db, &overridden), Some(judge_seat.context_id), "the override named judge: its live context");
        assert_eq!(senpai_of(&db, &delegated), Some(judge_seat.context_id), "the delegation named judge too");
        for column in ["reviewer_id", "director_id"] {
            assert!(!has_column(&db, "contexts", column), "contexts.{column} is dropped");
        }
        for table in ["approval_reviewer_delegations", "approval_reviewer_delegation_events"] {
            assert!(!has_table(&db, table), "{table} is dropped");
        }
        let reported: Vec<ContextId> = report_of(&db).into_iter().map(|(c, _, _)| c).collect();
        assert_eq!(reported, vec![overridden.context_id, delegated.context_id]);
        let old_reviewers: Vec<Option<Vec<u8>>> = db.conn.prepare("SELECT old_reviewer FROM senpai_migration_report ORDER BY recorded_at, context_id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<SqliteResult<_>>().unwrap();
        assert!(old_reviewers.iter().all(|r| r.as_deref() == Some(judge.as_bytes().as_slice())), "the report keeps who the old route named");
        for (ctx, reviewer) in [(&lane, banto), (&overridden, judge), (&delegated, judge)] {
            assert_eq!(db.route_review(ReviewQuery::ask(ctx.context_id, coder)).unwrap().reviewer(), reviewer, "{:?}", ctx.label);
        }

        senpai_open_steps(&db.conn).unwrap();
        assert_eq!(report_of(&db).len(), 2, "a second open changes nothing");
    }

    /// A database the step-1 kernel already mapped (the marker is set) still
    /// carries the columns and tables; opening drops them and leaves every
    /// senpai as it was. A migration that refuses drops nothing, so the old
    /// routing is still there to map on the next try.
    #[test]
    fn the_drop_follows_a_recorded_mapping_and_never_a_refused_one() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, nobody, amy] = seed_characters(&db, &["coder", "banto", "nobody", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let bad = old_context(&db, ws_id, "bad", Some(&seat), Some(coder), 3);
        set_old_routing(&db, &bad, Some(banto), Some(nobody));
        db.conn.execute("DELETE FROM kernel_migrations WHERE name = ?1", [KernelDb::SENPAI_MIGRATION]).unwrap();

        let error = senpai_open_steps(&db.conn).unwrap_err().to_string();
        assert!(error.contains("bad") && error.contains("nobody"), "{error}");
        assert!(has_column(&db, "contexts", "reviewer_id") && has_column(&db, "contexts", "director_id"), "a refused mapping drops nothing");
        assert!(has_table(&db, "approval_reviewer_delegations"));

        set_old_routing(&db, &bad, Some(banto), None);
        set_stored_senpai(&db, &bad, &seat);
        db.conn.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [KernelDb::SENPAI_MIGRATION]).unwrap();
        senpai_open_steps(&db.conn).unwrap();
        assert!(!has_column(&db, "contexts", "reviewer_id") && !has_column(&db, "contexts", "director_id"));
        assert!(!has_table(&db, "approval_reviewer_delegations") && !has_table(&db, "approval_reviewer_delegation_events"));
        assert_eq!(senpai_of(&db, &bad), Some(seat.context_id), "the recorded mapping is kept");
    }

    /// A context the migration cannot map refuses the whole migration, naming
    /// every such context. Nothing is written: no senpai, no report, no marker.
    #[test]
    fn senpai_migration_refuses_an_unmappable_context_and_names_each() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, nobody, gone, amy] = seed_characters(&db, &["coder", "banto", "nobody", "gone", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let fine = old_context(&db, ws_id, "fine", Some(&seat), Some(coder), 3);
        // An override naming a character with no console and no live context.
        let bad_override = old_context(&db, ws_id, "bad-override", Some(&seat), Some(coder), 4);
        set_old_routing(&db, &bad_override, Some(banto), Some(nobody));
        // An override naming a retired character: the old resolver refused every ask.
        let retired_override = old_context(&db, ws_id, "retired-override", Some(&seat), Some(coder), 5);
        set_old_routing(&db, &retired_override, Some(banto), Some(gone));
        assert!(db.retire_character(gone, 1).unwrap());
        // An archived context is never a reason to refuse.
        let archived_bad = old_context(&db, ws_id, "archived-bad", Some(&seat), Some(coder), 7);
        set_old_routing(&db, &archived_bad, Some(banto), Some(nobody));
        db.archive_context(archived_bad.context_id).unwrap();

        let error = run_senpai_migration(&db).unwrap_err().to_string();
        assert!(error.contains("bad-override") && error.contains("nobody"), "{error}");
        assert!(error.contains("retired-override") && error.contains("gone"), "{error}");
        assert!(!error.contains("archived-bad") && !error.contains("fine"), "{error}");
        assert!(error.contains("kj context move") || error.contains("senpai"), "{error}");

        assert_eq!(senpai_of(&db, &fine), None, "a refused migration writes nothing");
        assert_eq!(report_of(&db), vec![]);
        let marker: bool = db.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)", [KernelDb::SENPAI_MIGRATION], |r| r.get(0),
        ).unwrap();
        assert!(!marker, "so the next open tries again");
    }

    /// An archived context that cannot be mapped is left a top and reported;
    /// it never blocks the migration.
    #[test]
    fn senpai_migration_leaves_an_unmappable_archived_context_a_top_and_reports_it() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, nobody, amy] = seed_characters(&db, &["coder", "banto", "nobody", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
        let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
        let archived = old_context(&db, ws_id, "archived-bad", Some(&seat), Some(coder), 3);
        set_old_routing(&db, &archived, Some(banto), Some(nobody));
        db.archive_context(archived.context_id).unwrap();
        let archived_ok = old_context(&db, ws_id, "archived-ok", Some(&seat), Some(coder), 4);
        db.archive_context(archived_ok.context_id).unwrap();

        run_senpai_migration(&db).unwrap();
        assert_eq!(senpai_of(&db, &archived), None);
        assert_eq!(senpai_of(&db, &archived_ok), Some(seat.context_id), "archived contexts map like live ones");
        let report = report_of(&db);
        assert_eq!(report.len(), 1, "{report:?}");
        assert_eq!((report[0].0, report[0].1), (archived.context_id, None));
        assert!(report[0].2.contains("archived") && report[0].2.contains("nobody"), "{report:?}");
    }

    /// A kernel.db opened with no senpai marker and no report table maps its edges on `open`.
    #[test]
    fn opening_a_pre_senpai_database_adds_the_column_and_maps_the_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        let (lane, seat, root, amy) = {
            let db = KernelDb::open(&path).unwrap();
            let ws_id = setup_test_db(&db);
            let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
            db.update_character_root(amy, true).unwrap();
            let root = old_context(&db, ws_id, "amy", None, Some(amy), 1);
        set_stored_root_ctx(&db, amy, root.context_id);
            let seat = old_context(&db, ws_id, "banto", Some(&root), Some(banto), 2);
            let lane = old_context(&db, ws_id, "lane", Some(&seat), Some(coder), 3);
            db.conn.execute_batch(
                "DROP TABLE senpai_migration_report;
                 DELETE FROM kernel_migrations WHERE name IN ('senpai-edges-v1', 'instances-v1');",
            ).unwrap();
            (lane, seat, root, amy)
        };
        let db = KernelDb::open(&path).unwrap();
        assert_eq!(db.senpai_of(lane.context_id).unwrap(), Some(seat.context_id));
        assert_eq!(db.senpai_of(seat.context_id).unwrap(), Some(root.context_id));
        assert_eq!(db.lineage_root(lane.context_id).unwrap(), amy);
    }

    /// `open_read_only` reads an existing `kernel.db` without running
    /// `SCHEMA` or migrations — the lockout-recovery path
    /// (`kaijutsu-server list-characters`), which must work with the
    /// service stopped and must never contend with a live server's write
    /// connection.
    #[test]
    fn open_read_only_reads_an_existing_db_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        let character = PrincipalId::new();
        {
            let db = KernelDb::open(&path).unwrap();
            db.insert_character(&CharacterRow { context_type: None,
                principal_id: character,
                name: "hajime".to_string(),
                created_at: 1000,
                retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            })
            .unwrap();
        }

        let ro = KernelDb::open_read_only(&path).unwrap();
        let row = ro.get_character_by_name("hajime").unwrap().unwrap();
        assert_eq!(row.principal_id, character);

        // A write attempt through the read-only connection must fail loudly
        // rather than silently succeeding — the whole point of the mode.
        let err = ro
            .insert_character(&CharacterRow { context_type: None,
                principal_id: PrincipalId::new(),
                name: "intruder".to_string(),
                created_at: 2000,
                retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            })
            .unwrap_err();
        assert!(
            matches!(err, KernelDbError::Db(_)),
            "a write on a read-only connection must fail, got: {err}"
        );
    }

    /// `open_read_only` against a path with no database must fail loudly —
    /// there is nothing sensible to fall back to (`docs/character.md`'s
    /// "corruption, never a fallback" rule applies here too).
    #[test]
    fn open_read_only_fails_loudly_on_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-kernel.db");
        assert!(KernelDb::open_read_only(&missing).is_err());
    }

    // ── 23. Context shell CRUD ────────────────────────────────────────

    // ── 23b. Context tool bindings CRUD (Phase 5, D-54) ──────────────
    //
    // Normalized schema: parent `context_bindings` + `_instances` (ordered)
    // + `_names` (sticky map). The get path reconstructs `ContextToolBinding`
    // by joining; the upsert path writes all three tables transactionally.

    #[test]
    fn fork_copies_binding() {
        // Permissions follow the fork: copy_context_binding clones the parent's
        // loadout so a fork is not locked out under deny-by-default.
        use crate::mcp::Capability;
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let parent = make_context_row(Some("fork-parent"));
        let child = make_context_row(Some("fork-child"));
        insert_context_with_doc(&db, &parent, ws_id);
        insert_context_with_doc(&db, &child, ws_id);

        let mut b = ContextToolBinding::new();
        b.grant(Capability::AllInstances);
        b.grant(Capability::Facade("shell".into()));
        db.upsert_context_binding(parent.context_id, &b).unwrap();

        assert!(db.copy_context_binding(parent.context_id, child.context_id).unwrap());
        let loaded = db
            .get_context_binding(child.context_id)
            .unwrap()
            .expect("child inherited a binding");
        assert!(loaded.all_instances);
        assert!(loaded.allows(&Capability::Facade("shell".into())));
    }

    // ── Context egress (docs/egress.md) ─────────────────────────────

    #[test]
    fn validate_egress_host_accepts_dns_names_loopback_literals_and_star() {
        assert_eq!(validate_egress_host("crates.io").unwrap(), "crates.io");
        assert_eq!(validate_egress_host("LOCALHOST").unwrap(), "localhost");
        assert_eq!(validate_egress_host("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(validate_egress_host("::1").unwrap(), "::1");
        assert_eq!(validate_egress_host("0:0:0:0:0:0:0:1").unwrap(), "::1");
        assert_eq!(validate_egress_host("*").unwrap(), "*");
    }

    #[test]
    fn validate_egress_host_rejects_scheme_path_port_wildcard_empty_and_space() {
        for bad in ["https://x", "x/path", "x:443", "*.x.io", "", "x y"] {
            let err = validate_egress_host(bad).unwrap_err();
            assert!(matches!(err, KernelDbError::InvalidEgressHost(_)), "{bad}: {err}");
        }
    }

    // ── Fork inheritance ──────────────────────────────────────────────

    // ── 25. Workspace paths with read_only ────────────────────────────

    // ── 26. Fork context config (composite) ──────────────────────────

    #[test]
    fn fork_context_config_full() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("fork-src"));
        let tgt = make_context_row(Some("fork-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Set up source with shell config + env vars
        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/src/kaijutsu".into()),
            updated_at: now_millis(),
        })
        .unwrap();
        db.set_context_env(src.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
        db.set_context_env(src.context_id, "TERM", "xterm-256color")
            .unwrap();

        db.fork_context_config(src.context_id, tgt.context_id)
            .unwrap();

        // Shell config copied
        let shell = db.get_context_shell(tgt.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/src/kaijutsu".into()));

        // Env vars copied
        let vars = db.get_context_env(tgt.context_id).unwrap();
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn fork_context_config_empty_source() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("empty-src"));
        let tgt = make_context_row(Some("empty-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Fork from context with no config → no error, no data on target
        db.fork_context_config(src.context_id, tgt.context_id)
            .unwrap();

        assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
        assert!(db.get_context_env(tgt.context_id).unwrap().is_empty());
    }

    /// A fork that fails partway must leave NO partial config behind — a
    /// half-copied loadout strands a misconfigured (locked-out) context. The
    /// three copies share one transaction, so a failure on a later write must
    /// roll back the earlier ones. Force the env write (step 2) to abort after
    /// the shell write (step 1) has already executed, then prove the shell row
    /// never lands. Against the old per-write autocommit, the shell row would
    /// survive and this fails.
    #[test]
    fn fork_context_config_rolls_back_on_partial_failure() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("atomic-src"));
        let tgt = make_context_row(Some("atomic-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Source has shell config (copied first) AND an env var (copied second).
        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/work".into()),
            updated_at: now_millis(),
        })
        .unwrap();
        db.set_context_env(src.context_id, "RUST_LOG", "debug")
            .unwrap();

        // Booby-trap the env INSERT: a BEFORE INSERT trigger that aborts the
        // statement. Source reads are SELECTs and unaffected — only the
        // target's env write trips it, after the shell write has run in the tx.
        db.conn
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON context_env
                 BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();

        let result = db.fork_context_config(src.context_id, tgt.context_id);
        assert!(result.is_err(), "the aborted env write must surface as an error");

        // Remove the trap; SELECTs below don't fire it, but keep the db clean.
        db.conn.execute_batch("DROP TRIGGER boom;").unwrap();

        // The shell write succeeded *before* the abort, yet must be gone: the
        // transaction rolled back on drop. No partial fork survived.
        assert!(
            db.get_context_shell(tgt.context_id).unwrap().is_none(),
            "shell copy must roll back when a later fork write fails",
        );
        assert!(
            db.get_context_env(tgt.context_id).unwrap().is_empty(),
            "no env rows on a rolled-back fork",
        );
    }

    /// The composite fork — document row + context row + shell/env/binding + attachment
    /// copy — lands in one shot. Insert a source with full config, fork it into a
    /// brand-new target via `insert_forked_context`, and prove every piece arrived.
    #[test]
    fn insert_forked_context_creates_rows_and_copies_config() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("ifc-src"));
        insert_context_with_doc(&db, &src, ws_id);

        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/work/kaijutsu".into()),
            updated_at: now_millis(),
        })
        .unwrap();
        db.set_context_env(src.context_id, "RUST_LOG", "debug").unwrap();
        db.upsert_context_binding(
            src.context_id,
            &binding_with(&["builtin.file"], &[("read", "builtin.file", "file_read")]),
        )
        .unwrap();
        // Attachment carries the track binding into the child — the child joins
        // the same clock domain the parent is on (docs/tracks.md §3).
        db.upsert_track(&make_track("bass", 500)).unwrap();
        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 8,
            rotate_every_phrases: Some(4),
            ooda_armed: true,
            ..make_attachment("bass", src.context_id)
        })
        .unwrap();

        // The target is a fresh context that does not yet exist in any table.
        let tgt = make_context_row(Some("ifc-tgt"));
        db.insert_forked_context(&tgt, ws_id, src.context_id, Some("/work/kaijutsu")).unwrap();

        // Both the document row and the context row were created.
        assert!(db.get_document(tgt.context_id).unwrap().is_some());
        assert!(db.get_context(tgt.context_id).unwrap().is_some());

        // The given cwd landed; env + binding followed the fork.
        assert_eq!(
            db.get_context_shell(tgt.context_id).unwrap().unwrap().cwd,
            Some("/work/kaijutsu".into()),
        );
        assert_eq!(db.get_context_env(tgt.context_id).unwrap().len(), 1);
        assert!(db.get_context_binding(tgt.context_id).unwrap().is_some());

        // Attachment followed the fork — child joins the same track (bass), keeping
        // the wakeup divisor and rotate cadence from the parent.
        let child_att = db.get_attachment("bass", tgt.context_id).unwrap().unwrap();
        assert_eq!(child_att.track_id, "bass", "child attached to parent's track");
        assert_eq!(child_att.wakeup_every, 8, "wakeup divisor carried");
        assert_eq!(child_att.rotate_every_phrases, Some(4), "rotate cadence carried");
        assert!(child_att.ooda_armed, "ooda_armed carried");
    }

    /// The whole fork is all-or-nothing: if a *later* write (the config copy)
    /// fails, the context + document rows inserted *earlier* in the same
    /// transaction must roll back too. Otherwise a fork could strand a
    /// committed-but-misconfigured context — the exact gap this method closes.
    /// Booby-trap the target's env INSERT, then prove neither row survived.
    /// Against the old `insert_context_with_document` + `fork_context_config`
    /// pair (two separate autocommits), the context row would persist.
    #[test]
    fn insert_forked_context_rolls_back_rows_on_config_failure() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("ifc-atomic-src"));
        insert_context_with_doc(&db, &src, ws_id);
        // Source has an env var, so the fork will attempt a target env write.
        db.set_context_env(src.context_id, "RUST_LOG", "debug").unwrap();

        // Abort any INSERT into context_env. The source's var is already in
        // place (inserted above); only the fork's target write trips this,
        // after the context + document rows have run in the transaction.
        db.conn
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON context_env
                 BEGIN SELECT RAISE(ABORT, 'forced'); END;",
            )
            .unwrap();

        let tgt = make_context_row(Some("ifc-atomic-tgt"));
        let result = db.insert_forked_context(&tgt, ws_id, src.context_id, None);
        assert!(result.is_err(), "the aborted env write must surface as an error");

        db.conn.execute_batch("DROP TRIGGER boom;").unwrap();

        // The context + document inserts succeeded earlier in the tx, yet must
        // be gone: the transaction rolled back on drop. No half-built fork.
        assert!(
            db.get_context(tgt.context_id).unwrap().is_none(),
            "context row must roll back when the config copy fails",
        );
        assert!(
            db.get_document(tgt.context_id).unwrap().is_none(),
            "document row must roll back when the config copy fails",
        );
    }

    /// A fork keeps its parent's loadout but not the grants for MCP servers
    /// a client declared for the parent alone.
    #[test]
    fn insert_forked_context_drops_context_scoped_mcp_grants() {
        use crate::mcp::{Capability, ContextToolBinding, InstanceId};
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("ifc-scoped-src"));
        insert_context_with_doc(&db, &src, ws_id);
        let scoped = InstanceId::new(format!("context.{}.fixture", src.context_id.to_hex()));
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::AllInstances);
        binding.grant(Capability::Instance(scoped.clone()));
        db.upsert_context_binding(src.context_id, &binding).unwrap();

        let tgt = make_context_row(Some("ifc-scoped-tgt"));
        db.insert_forked_context(&tgt, ws_id, src.context_id, None).unwrap();

        let child = db.get_context_binding(tgt.context_id).unwrap().unwrap();
        assert!(child.all_instances, "the loadout follows the fork");
        assert!(
            !child.allows(&Capability::Instance(scoped)),
            "a parent's context-scoped MCP grant must not follow the fork"
        );
    }

    // ── 27. Context workspace paths query ─────────────────────────────

    // ── 28. Workspace path permission checking ────────────────────────

    // ── 29. Time-well ring placement (promote/demote/pause) ────────────

    // ── Roster: dropping the legacy value-enum CHECKs ───────────────────
    // Rust-API coverage (upsert/reconcile/restart-shaped/generation/clock
    // skew) lives in `roster.rs`'s own test module. These exercise the
    // schema directly, the same split approval-ledger's `schema.rs` uses —
    // but where that split used to mean "the CHECK rejects what the Rust
    // API never sends," these four tables no longer carry that CHECK at
    // all (`drop_legacy_value_enum_checks`'s doc comment states the
    // ruling). `RosterEntity::from_parts`, `LivenessKind::parse`, and
    // `Availability::parse` (`roster.rs`) are the only gates left.

    fn put_roster_entity_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_entity_legacy (
                 entity_kind   TEXT NOT NULL CHECK (entity_kind IN ('principal', 'context')),
                 entity_id     BLOB NOT NULL,
                 label         TEXT,
                 first_seen_at INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 PRIMARY KEY (entity_kind, entity_id)
             );
             INSERT INTO roster_entity_legacy (entity_kind, entity_id, label, first_seen_at)
                 SELECT entity_kind, entity_id, label, first_seen_at FROM roster_entity;
             DROP TABLE roster_entity;
             ALTER TABLE roster_entity_legacy RENAME TO roster_entity;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    fn put_roster_presence_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_presence_legacy (
                 source          TEXT NOT NULL,
                 source_local_id TEXT NOT NULL,
                 entity_kind     TEXT NOT NULL,
                 entity_id       BLOB NOT NULL,
                 liveness_kind   TEXT NOT NULL
                     CHECK (liveness_kind IN ('bound', 'recent', 'attested')),
                 host            TEXT,
                 pid             INTEGER,
                 proc_start      INTEGER,
                 first_seen_at   INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
             );
             INSERT INTO roster_presence_legacy
                 (source, source_local_id, entity_kind, entity_id, liveness_kind,
                  host, pid, proc_start, first_seen_at)
                 SELECT source, source_local_id, entity_kind, entity_id, liveness_kind,
                        host, pid, proc_start, first_seen_at
                   FROM roster_presence;
             DROP TABLE roster_presence;
             ALTER TABLE roster_presence_legacy RENAME TO roster_presence;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    fn put_roster_activity_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_activity_legacy (
                 source          TEXT    NOT NULL,
                 source_local_id TEXT    NOT NULL,
                 live            INTEGER NOT NULL CHECK (live IN (0, 1)),
                 observed_at     INTEGER,
                 recorded_at     INTEGER NOT NULL,
                 PRIMARY KEY (source, source_local_id),
                 FOREIGN KEY (source, source_local_id)
                     REFERENCES roster_presence(source, source_local_id) ON DELETE CASCADE
             );
             INSERT INTO roster_activity_legacy (source, source_local_id, live, observed_at, recorded_at)
                 SELECT source, source_local_id, live, observed_at, recorded_at FROM roster_activity;
             DROP TABLE roster_activity;
             ALTER TABLE roster_activity_legacy RENAME TO roster_activity;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    fn put_roster_status_back_on_its_legacy_shape(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE roster_status_legacy (
                 entity_kind  TEXT NOT NULL,
                 entity_id    BLOB NOT NULL,
                 status_text  TEXT,
                 availability TEXT NOT NULL CHECK (availability IN ('active', 'idle', 'away', 'dnd')),
                 observed_at  INTEGER,
                 recorded_at  INTEGER NOT NULL,
                 PRIMARY KEY (entity_kind, entity_id),
                 FOREIGN KEY (entity_kind, entity_id)
                     REFERENCES roster_entity(entity_kind, entity_id) ON DELETE CASCADE
             );
             INSERT INTO roster_status_legacy
                 (entity_kind, entity_id, status_text, availability, observed_at, recorded_at)
                 SELECT entity_kind, entity_id, status_text, availability, observed_at, recorded_at
                   FROM roster_status;
             DROP TABLE roster_status;
             ALTER TABLE roster_status_legacy RENAME TO roster_status;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    }

    /// A database carrying `roster_entity`'s legacy `entity_kind` CHECK must
    /// lose it on the next open (`apply_additive_migrations`), keep the row
    /// it already had, and accept a kind the old CHECK rejected. Falsified
    /// by reverting `drop_legacy_value_enum_checks` to a no-op: the final
    /// INSERT below then failed with a CHECK violation.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_entity_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, label, first_seen_at)
                 VALUES ('principal', ?1, 'kept', 1)",
                params![blob_param(PrincipalId::new().as_bytes())],
            )
            .unwrap();
        put_roster_entity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (entity_kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let label: String = db
            .conn
            .query_row("SELECT label FROM roster_entity WHERE entity_kind = 'principal'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(label, "kept");

        // Neither 'principal' nor 'context' — never admitted by the retired
        // CHECK. `RosterEntity::from_parts` (roster.rs) is the sole gate now.
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('service', ?1, 1)",
                params![blob_param(&[7u8; 16])],
            )
            .unwrap();
    }

    /// Same contract as the `roster_entity` test above, for
    /// `roster_presence.liveness_kind`.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_presence_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('kept-source', 'kept-id', 'principal', ?1, 'bound', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        put_roster_presence_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_presence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (liveness_kind IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let kept: String = db
            .conn
            .query_row("SELECT liveness_kind FROM roster_presence WHERE source = 'kept-source'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "bound");

        // 'heard' is the design table's fourth kind, deliberately never
        // admitted by the retired CHECK (no v1 producer). It now inserts
        // cleanly — `LivenessKind::parse` (roster.rs) is the sole gate.
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('new-source', 'new-id', 'principal', ?1, 'heard', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
    }

    /// Same contract as the `roster_entity` test above, for
    /// `roster_activity.live`.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_activity_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('kept-source', 'kept-id', 'principal', ?1, 'bound', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_activity (source, source_local_id, live, recorded_at)
                 VALUES ('kept-source', 'kept-id', 1, 1)",
                [],
            )
            .unwrap();
        put_roster_activity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_activity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (live IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let kept: i64 = db
            .conn
            .query_row("SELECT live FROM roster_activity WHERE source = 'kept-source'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, 1);

        // `live` was a strict 0/1 boolean under the retired CHECK. The only
        // writer (`roster_reconcile_presence`) always writes 0 or 1; this
        // pins that the schema itself no longer enforces it.
        db.conn
            .execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind, first_seen_at)
                 VALUES ('new-source', 'new-id', 'principal', ?1, 'bound', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_activity (source, source_local_id, live, recorded_at)
                 VALUES ('new-source', 'new-id', 2, 1)",
                [],
            )
            .unwrap();
    }

    /// Same contract as the `roster_entity` test above, for
    /// `roster_status.availability`.
    #[test]
    fn drop_legacy_value_enum_checks_drops_the_roster_status_check_and_keeps_rows() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_status (entity_kind, entity_id, availability, recorded_at)
                 VALUES ('principal', ?1, 'active', 1)",
                params![blob_param(principal.as_bytes())],
            )
            .unwrap();
        put_roster_status_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'roster_status'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("CHECK (availability IN ("), "the value-enum CHECK must be gone, got: {sql}");
        let kept: String = db
            .conn
            .query_row("SELECT availability FROM roster_status WHERE entity_kind = 'principal'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "active");

        // 'sideways' was never one of the retired CHECK's four values. It
        // now inserts cleanly — `Availability::parse` (roster.rs) is the
        // sole gate.
        let second = PrincipalId::new();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(second.as_bytes())],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_status (entity_kind, entity_id, availability, recorded_at)
                 VALUES ('principal', ?1, 'sideways', 1)",
                params![blob_param(second.as_bytes())],
            )
            .unwrap();
    }

    /// The hazard `drop_legacy_value_enum_checks` exists to avoid:
    /// `roster_entity` is the FK parent of `roster_presence` and
    /// `roster_status` (`ON DELETE CASCADE`), and `roster_presence` is in
    /// turn the parent of `roster_activity`. With foreign keys enforced —
    /// the connection's normal state, set by `init_connection` — a naive
    /// rebuild of `roster_entity` performs an implicit `DELETE FROM` under
    /// `DROP TABLE` and fires every one of those cascades, emptying the
    /// live roster. Falsified by removing the `PRAGMA foreign_keys = OFF`
    /// window from `drop_legacy_value_enum_checks`: every child row seeded
    /// below was gone after `apply_additive_migrations`.
    #[test]
    fn drop_legacy_value_enum_checks_does_not_cascade_delete_the_live_roster() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("roster-cascade"));
        insert_context_with_doc(&db, &row, ws_id);
        let entity = RosterEntity::Context(row.context_id);

        db.roster_reconcile_presence(
            "cascade-source",
            LivenessKind::Bound,
            &[PresenceSnapshotRow {
                source_local_id: "cascade-peer".into(),
                entity,
                entity_label: Some("cascade".into()),
                host: Some("localhost".into()),
                pid: Some(4321),
                proc_start: Some(1000),
                observed_at: Some(1000),
                live: true,
            }],
            1000,
        )
        .unwrap();
        db.roster_write_status(entity, Some("cascade"), Some("hi"), Availability::Active, Some(1000), 1000)
            .unwrap();

        let count = |table: &str| -> i64 {
            db.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        // Confirm the fixture actually populated every child table before
        // touching anything, so a later count of 0 unambiguously means the
        // migration deleted rows rather than never having seen any.
        assert_eq!(count("roster_presence"), 1);
        assert_eq!(count("roster_activity"), 1);
        assert_eq!(count("roster_status"), 1);

        put_roster_entity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        assert_eq!(count("roster_entity"), 1, "the entity row must survive its own rebuild");
        assert_eq!(
            count("roster_presence"),
            1,
            "roster_entity's rebuild must not cascade into roster_presence"
        );
        assert_eq!(
            count("roster_activity"),
            1,
            "the cascade must not reach two levels down into roster_activity"
        );
        assert_eq!(count("roster_status"), 1, "roster_entity's rebuild must not cascade into roster_status");
    }

    /// `DROP TABLE` takes a table's triggers with it. After a
    /// `roster_entity` rebuild, `roster_entity_context_must_exist` must
    /// both still be listed in `sqlite_master` and still fire — checking
    /// only the listing would miss a trigger body silently changed by a
    /// bad `SCHEMA` edit. Falsified by skipping the post-rebuild `SCHEMA`
    /// re-run in `drop_legacy_value_enum_checks`: the trigger disappeared
    /// and the phantom insert below succeeded instead of erroring.
    #[test]
    fn drop_legacy_value_enum_checks_restores_the_roster_entity_trigger() {
        let db = KernelDb::temporary().unwrap();
        put_roster_entity_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let trigger_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'trigger' AND name = 'roster_entity_context_must_exist'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger_count, 1, "the joinability trigger must survive the rebuild");

        let phantom = ContextId::new();
        let err = db
            .conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('context', ?1, 1)",
                params![blob_param(phantom.as_bytes())],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("never minted"),
            "expected the joinability trigger's message, got: {err}"
        );
    }

    /// `DROP TABLE` takes a table's indexes with it too. After a
    /// `roster_presence` rebuild, `idx_roster_presence_entity` must still
    /// exist. Falsified the same way as the trigger test above.
    #[test]
    fn drop_legacy_value_enum_checks_restores_the_roster_presence_index() {
        let db = KernelDb::temporary().unwrap();
        put_roster_presence_back_on_its_legacy_shape(&db.conn);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let index_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'index' AND name = 'idx_roster_presence_entity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1, "idx_roster_presence_entity must survive the rebuild");
    }

    /// A real `documents` row, because `doc_snapshots.document_id` is a
    /// foreign key and a snapshot for a document that does not exist would
    /// not be a case this migration ever meets.
    fn setup_snapshot_document(db: &KernelDb) -> ContextId {
        let ws_id = setup_test_db(db);
        let doc_id = ContextId::new();
        db.insert_document(&DocumentRow {
            document_id: doc_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        })
        .unwrap();
        doc_id
    }

    /// Bring `doc_snapshots` back to the shape that carried `content`, and
    /// seed a row through it, so the next open has something real to migrate.
    fn put_doc_snapshots_back_with_its_content_column(conn: &Connection, doc: ContextId) {
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE doc_snapshots;
             CREATE TABLE doc_snapshots (
                 document_id BLOB    NOT NULL PRIMARY KEY,
                 seq         INTEGER NOT NULL,
                 version     INTEGER NOT NULL,
                 state       BLOB    NOT NULL,
                 content     TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
                     DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
                 FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
             );
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO doc_snapshots (document_id, seq, version, state, content, created_at)
             VALUES (?1, 7, 3, X'DEADBEEF', 'the derived copy', 1)",
            params![blob_param(doc.as_bytes())],
        )
        .unwrap();
    }

    /// A database still carrying `doc_snapshots.content` loses the column on
    /// the next open, and the row it held keeps every other value. The text
    /// was recoverable from `state` all along, so dropping it loses nothing
    /// a reader could have wanted.
    #[test]
    fn the_doc_snapshots_content_column_is_dropped_and_the_row_survives() {
        let db = KernelDb::temporary().unwrap();
        let doc = setup_snapshot_document(&db);
        put_doc_snapshots_back_with_its_content_column(&db.conn, doc);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'doc_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sql.contains("content"), "the content column must be gone, got: {sql}");

        let row = db.load_latest_snapshot(doc).unwrap().expect("the row must survive the drop");
        assert_eq!(row.seq, 7, "seq must survive");
        assert_eq!(row.version, 3, "version must survive");
        assert_eq!(row.state, vec![0xDE, 0xAD, 0xBE, 0xEF], "state must survive");
        assert_eq!(row.created_at, 1, "created_at must survive, and must not shift a column left");
    }

    /// The drop is guarded, so a second open does not re-run it — an
    /// unguarded `ALTER TABLE ... DROP COLUMN` on a column that is already
    /// gone is an error, not a no-op.
    #[test]
    fn dropping_the_doc_snapshots_content_column_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
        let doc = setup_snapshot_document(&db);
        put_doc_snapshots_back_with_its_content_column(&db.conn, doc);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
    }

    /// Bring `contexts.consent_mode` and `presets.consent_mode` back, with a
    /// non-default value on each row, so the next open has something real to
    /// migrate.
    fn put_consent_mode_columns_back(conn: &Connection, ctx: ContextId, preset: PresetId) {
        conn.execute(
            "ALTER TABLE contexts ADD COLUMN consent_mode TEXT NOT NULL DEFAULT 'collaborative'",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE presets ADD COLUMN consent_mode TEXT NOT NULL DEFAULT 'collaborative'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE contexts SET consent_mode = 'autonomous' WHERE context_id = ?1",
            params![blob_param(ctx.as_bytes())],
        )
        .unwrap();
        conn.execute(
            "UPDATE presets SET consent_mode = 'autonomous' WHERE preset_id = ?1",
            params![blob_param(preset.as_bytes())],
        )
        .unwrap();
    }

    /// A database still carrying `contexts.consent_mode` and
    /// `presets.consent_mode` loses both columns on the next open, and the
    /// rows they held keep every other value. Nothing reads the field any
    /// more — the migration exists only to leave an old database loadable.
    #[test]
    fn the_consent_mode_columns_are_dropped_and_rows_survive() {
        let db = KernelDb::temporary().unwrap();
        let ws = setup_test_db(&db);
        let row = make_context_row(Some("consent-drop-ctx"));
        let ctx = row.context_id;
        insert_context_with_doc(&db, &row, ws);
        let preset = insert_test_preset(&db, "consent-drop-preset");

        put_consent_mode_columns_back(&db.conn, ctx, preset);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();

        let contexts_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'contexts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !contexts_sql.contains("consent_mode"),
            "contexts.consent_mode must be gone, got: {contexts_sql}"
        );
        let presets_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'presets'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !presets_sql.contains("consent_mode"),
            "presets.consent_mode must be gone, got: {presets_sql}"
        );

        let loaded = db.get_context(ctx).unwrap().expect("the context row must survive the drop");
        assert_eq!(loaded.label, Some("consent-drop-ctx".to_string()));
        let loaded_preset =
            db.get_preset(preset).unwrap().expect("the preset row must survive the drop");
        assert_eq!(loaded_preset.label, "consent-drop-preset");
    }

    /// The drop is guarded, so a second open does not re-run it — an
    /// unguarded `ALTER TABLE ... DROP COLUMN` on a column that is already
    /// gone is an error, not a no-op.
    #[test]
    fn dropping_the_consent_mode_columns_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
        let ws = setup_test_db(&db);
        let row = make_context_row(Some("consent-drop-idempotent"));
        let ctx = row.context_id;
        insert_context_with_doc(&db, &row, ws);
        let preset = insert_test_preset(&db, "consent-drop-idempotent-preset");
        put_consent_mode_columns_back(&db.conn, ctx, preset);

        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
    }

    /// Every rebuild in `rebuild_value_enum_tables` carries its table's
    /// shape a second time, beside `SCHEMA`'s own copy of it. A rebuilt
    /// table must therefore end up with the shape a fresh database gets:
    /// the same columns, types, nullability, defaults and primary key, and
    /// the same foreign keys. An edit to `SCHEMA` that forgets its rebuild
    /// spec fails here, instead of silently dropping a column the next time
    /// an older kernel opens its database.
    #[test]
    fn a_rebuilt_table_has_the_shape_a_fresh_database_gets() {
        fn columns(conn: &Connection, table: &str) -> Vec<String> {
            conn.prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |row| {
                    Ok(format!(
                        "{}|{}|{}|{}|{}",
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        row.get::<_, i64>(5)?,
                    ))
                })
                .unwrap()
                .collect::<SqliteResult<Vec<_>>>()
                .unwrap()
        }
        fn foreign_keys(conn: &Connection, table: &str) -> Vec<String> {
            conn.prepare(&format!("PRAGMA foreign_key_list({table})"))
                .unwrap()
                .query_map([], |row| {
                    Ok(format!(
                        "{}|{}|{}|{}",
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        row.get::<_, String>(6)?,
                    ))
                })
                .unwrap()
                .collect::<SqliteResult<Vec<_>>>()
                .unwrap()
        }

        let fresh = KernelDb::temporary().unwrap();
        let rebuilt = KernelDb::temporary().unwrap();
        put_backends_back_on_its_pre_codex_shape(&rebuilt.conn);
        put_roster_entity_back_on_its_legacy_shape(&rebuilt.conn);
        put_roster_presence_back_on_its_legacy_shape(&rebuilt.conn);
        put_roster_activity_back_on_its_legacy_shape(&rebuilt.conn);
        put_roster_status_back_on_its_legacy_shape(&rebuilt.conn);

        KernelDb::apply_additive_migrations(&rebuilt.conn).unwrap();

        for table in
            ["backends", "roster_entity", "roster_presence", "roster_activity", "roster_status"]
        {
            assert_eq!(
                columns(&rebuilt.conn, table),
                columns(&fresh.conn, table),
                "{table}'s rebuild spec has drifted from SCHEMA"
            );
            assert_eq!(
                foreign_keys(&rebuilt.conn, table),
                foreign_keys(&fresh.conn, table),
                "{table}'s rebuild spec lost or changed a foreign key"
            );
        }
    }

    /// The joinability guarantee at the schema level: an `entity_kind =
    /// 'context'` row must name a context this kernel actually minted. A
    /// phantom id (never inserted into `contexts`) is refused outright.
    #[test]
    fn roster_entity_context_must_exist_trigger_refuses_a_phantom_context() {
        let db = KernelDb::temporary().unwrap();
        let _ws_id = setup_test_db(&db);
        let phantom = ContextId::new();
        let err = db
            .conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('context', ?1, 1)",
                params![blob_param(phantom.as_bytes())],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("never minted"),
            "expected the joinability trigger's message, got: {err}"
        );
    }

    /// A `principal` entity has no backing table to check against — the
    /// trigger's `WHEN` is scoped to `entity_kind = 'context'` only, so an
    /// arbitrary principal id must insert cleanly (documented in
    /// `roster.rs`'s module doc as Rust-API-only discipline for that half).
    #[test]
    fn roster_entity_principal_kind_is_not_checked_against_any_table() {
        let db = KernelDb::temporary().unwrap();
        db.conn
            .execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, first_seen_at)
                 VALUES ('principal', ?1, 1)",
                params![blob_param(PrincipalId::new().as_bytes())],
            )
            .unwrap();
    }

    // ── 31. Well-known context registry (docs/drifting-dead-letters.md, slice 1) ──

    /// Write one `documents` row per legacy tag, bypassing `DocKind` so the
    /// retired spellings reach SQLite verbatim. Returns the row ids in the
    /// order the tags were given.
    fn insert_raw_kind_docs(db: &KernelDb, ws_id: WorkspaceId, tags: &[&str]) -> Vec<ContextId> {
        tags.iter()
            .map(|tag| {
                let id = ContextId::new();
                db.conn
                    .execute(
                        "INSERT INTO documents (
                            document_id, workspace_id, doc_kind,
                            language, path, created_at, created_by
                        ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                        params![
                            blob_param(id.as_bytes()),
                            blob_param(ws_id.as_bytes()),
                            tag,
                            if *tag == "code" { Some("rust") } else { None },
                            now_millis(),
                            blob_param(PrincipalId::system().as_bytes()),
                        ],
                    )
                    .unwrap();
                id
            })
            .collect()
    }

    fn raw_doc_kind(db: &KernelDb, id: ContextId) -> String {
        db.conn
            .query_row(
                "SELECT doc_kind FROM documents WHERE document_id = ?1",
                params![blob_param(id.as_bytes())],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// `code`, `text`, and `config` collapse into `file`, and no row is lost:
    /// the migration rewrites a tag, it never deletes or inserts.
    #[test]
    fn migrate_doc_kind_file_collapse_remaps_legacy_tags() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let ids = insert_raw_kind_docs(&db, ws_id, &["code", "text", "config"]);
        let before = db.list_documents().unwrap().len();

        KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();

        for id in &ids {
            assert_eq!(raw_doc_kind(&db, *id), "file");
            let row = db.get_document(*id).unwrap().unwrap();
            assert_eq!(row.doc_kind, DocKind::File);
        }
        assert_eq!(
            db.list_documents().unwrap().len(),
            before,
            "the migration rewrites tags; it must not drop a row"
        );
    }

    /// `conversation` and `symlink` rows survive the collapse untouched — the
    /// `WHERE` clause names three tags and only those three.
    #[test]
    fn migrate_doc_kind_file_collapse_leaves_conversation_and_symlink() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let ids = insert_raw_kind_docs(&db, ws_id, &["conversation", "symlink"]);
        let before = db.list_documents().unwrap().len();

        KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();

        assert_eq!(raw_doc_kind(&db, ids[0]), "conversation");
        assert_eq!(raw_doc_kind(&db, ids[1]), "symlink");
        assert_eq!(
            db.get_document(ids[0]).unwrap().unwrap().doc_kind,
            DocKind::Conversation
        );
        assert_eq!(
            db.get_document(ids[1]).unwrap().unwrap().doc_kind,
            DocKind::Symlink
        );
        assert_eq!(db.list_documents().unwrap().len(), before);
    }

    /// Running the collapse twice is safe — the second run matches zero rows,
    /// which is why it carries no `kernel_migrations` marker.
    #[test]
    fn migrate_doc_kind_file_collapse_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let ids = insert_raw_kind_docs(&db, ws_id, &["code", "text", "config"]);

        KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();
        KernelDb::migrate_doc_kind_file_collapse(&db.conn).unwrap();

        for id in &ids {
            assert_eq!(raw_doc_kind(&db, *id), "file");
        }
        assert_eq!(db.list_documents().unwrap().len(), 3);
    }

    /// The backfill migration (`migrate_well_known_lost_found`) adopts a
    /// pre-existing `label = 'lost+found'` row into the registry, and running
    /// it again (as every `open()`/`temporary()` does) is a no-op.
    #[test]
    fn migrate_well_known_lost_found_backfills_and_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let legacy = make_context_row(Some("lost+found"));
        insert_context_with_doc(&db, &legacy, ws_id);

        // Pre-migration: nothing in the registry yet (temporary() already ran
        // the migration once before this row existed).
        assert_eq!(db.well_known_context(WellKnownRole::LostFound).unwrap(), None);

        KernelDb::migrate_well_known_lost_found(&db.conn).unwrap();
        assert_eq!(
            db.well_known_context(WellKnownRole::LostFound).unwrap(),
            Some(legacy.context_id)
        );

        // A second run (idempotent — INSERT OR IGNORE) must not disturb the
        // mapping, even if a rotation happened in between.
        let rotated = make_context_row(Some("lost+found-2"));
        insert_context_with_doc(&db, &rotated, ws_id);
        db.set_well_known_context(WellKnownRole::LostFound, rotated.context_id).unwrap();

        KernelDb::migrate_well_known_lost_found(&db.conn).unwrap();
        assert_eq!(
            db.well_known_context(WellKnownRole::LostFound).unwrap(),
            Some(rotated.context_id),
            "re-running the backfill must not clobber a later rotation"
        );
    }

    /// `temporary()` must open a real file on disk, not `:memory:` — and that
    /// file (and the directory holding it) must be gone once nothing holds
    /// the `KernelDb` anymore. Asserted against the filesystem directly, not
    /// against any in-process bookkeeping.
    #[test]
    fn temporary_writes_a_real_file_and_removes_it_on_drop() {
        let db = KernelDb::temporary().unwrap();
        let dir = db._temp_dir.as_ref().unwrap().path().to_path_buf();
        let db_path = dir.join("kernel.db");
        assert!(db_path.is_file(), "temporary() must back its handle with a real file");

        drop(db);

        assert!(
            !dir.exists(),
            "the temp directory must be removed once the KernelDb handle drops"
        );
    }

    // ========================================================================
    // insert_document_with_op — the row and the first op commit together or
    // not at all. A row that commits alone is a document with no snapshot and
    // no oplog: it loads empty and every read of it fails.
    // ========================================================================

    // ========================================================================
    // `context_state` and `archived_at` are written together. Before this,
    // archiving stamped only the timestamp, so every archived context read
    // back as `live` and the enum lied to anyone who trusted it alone.
    // ========================================================================

    /// The backfill for databases written before archiving set the state.
    /// The disagreeing row is planted the way those rows were made — the
    /// timestamp alone.
    #[test]
    fn migrate_archived_context_state_backfills_a_disagreeing_row() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("legacy-archived"));
        insert_context_with_doc(&db, &row, ws_id);
        db.conn
            .execute(
                "UPDATE contexts SET archived_at = ?1 WHERE context_id = ?2",
                params![now_millis(), blob_param(row.context_id.as_bytes())],
            )
            .unwrap();
        assert_eq!(
            db.get_context(row.context_id).unwrap().unwrap().context_state,
            ContextState::Live,
            "the planted row must start out disagreeing, or this proves nothing"
        );

        KernelDb::migrate_archived_context_state(&db.conn).unwrap();
        assert_eq!(
            db.get_context(row.context_id).unwrap().unwrap().context_state,
            ContextState::Archived
        );

        // A live context is not swept up by it, and a second run is a no-op.
        let live = make_context_row(Some("still-live"));
        insert_context_with_doc(&db, &live, ws_id);
        KernelDb::migrate_archived_context_state(&db.conn).unwrap();
        assert_eq!(
            db.get_context(live.context_id).unwrap().unwrap().context_state,
            ContextState::Live
        );
        assert_eq!(
            db.get_context(row.context_id).unwrap().unwrap().context_state,
            ContextState::Archived
        );
    }

}

#[cfg(test)]
mod label_index_tests {
    //! Labels are unique among live contexts; an archived context keeps its
    //! name and frees it.
    use super::*;

    fn index_sql(db: &KernelDb) -> String {
        db.conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_contexts_label'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn the_old_index_shape_is_rebuilt_at_open() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        db.conn
            .execute_batch(
                "DROP INDEX idx_contexts_label;
                 CREATE UNIQUE INDEX idx_contexts_label
                     ON contexts(label) WHERE label IS NOT NULL;",
            )
            .unwrap();
        assert!(!index_sql(&db).contains("archived_at IS NULL"), "the planted index must be the old shape");

        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        assert!(index_sql(&db).contains("archived_at IS NULL"));

        let old = make_context_row(Some("ROOT"));
        insert_context_with_doc(&db, &old, ws_id);
        assert!(db.archive_context(old.context_id).unwrap());
        insert_context_with_doc(&db, &make_context_row(Some("ROOT")), ws_id);

        // A second run is a no-op.
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        assert!(index_sql(&db).contains("archived_at IS NULL"));
    }
}
