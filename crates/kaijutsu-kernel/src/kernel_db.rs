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
//! revisit only when it's an actual, observed problem. Tracked, with the
//! connection-pool angle, in `docs/issues.md` (Persistence & Sync) and
//! `docs/architecture/kernel.md`. Don't pre-emptively refactor.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Result as SqliteResult, params};
use tracing::{info, warn};

use kaijutsu_types::{
    BlockId, ConsentMode, ContextId, ContextState, DocKind, EdgeKind, ForkKind, KernelId, PresetId,
    PrincipalId, WorkspaceId,
};

use crate::llm::stream::{CacheTarget, CacheTtl};
use crate::mcp::binding::ContextToolBinding;
use crate::mcp::types::InstanceId;

// ============================================================================
// Error type
// ============================================================================

/// Errors from KernelDb operations.
#[derive(Debug, thiserror::Error)]
pub enum KernelDbError {
    /// Underlying SQLite error.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// Entity not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Label already in use for this kernel.
    #[error("label conflict: {0}")]
    LabelConflict(String),

    /// Label contains forbidden characters.
    #[error("invalid label: {0}")]
    InvalidLabel(String),

    /// Structural edge would create a cycle.
    #[error("cycle detected: adding this edge would create a cycle")]
    CycleDetected,

    /// General validation failure.
    #[error("validation error: {0}")]
    Validation(String),
}

pub type KernelDbResult<T> = Result<T, KernelDbError>;

// ============================================================================
// Row types
// ============================================================================

/// A context row — superset of `Context` + `ContextHandle`.
#[derive(Debug, Clone)]
pub struct ContextRow {
    pub context_id: ContextId,
    pub label: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub consent_mode: ConsentMode,
    pub context_state: ContextState,
    pub context_type: String,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub forked_from: Option<ContextId>,
    pub fork_kind: Option<ForkKind>,
    pub archived_at: Option<i64>,
    /// Unix-millis of the explicit `conclude` act, or `None` if still open.
    pub concluded_at: Option<i64>,
    pub workspace_id: Option<WorkspaceId>,
    pub preset_id: Option<PresetId>,
}

impl ContextRow {
    /// Convert to the lightweight `kaijutsu_types::Context`.
    pub fn to_context(&self) -> kaijutsu_types::Context {
        kaijutsu_types::Context {
            id: self.context_id,
            label: self.label.clone(),
            forked_from: self.forked_from,
            created_by: self.created_by,
            created_at: self.created_at as u64,
        }
    }
}

/// A persisted hook entry. Flat shape so the rusqlite row readers map
/// directly; the broker reconstructs the live `HookEntry` (including
/// resolving `action_builtin_name` against `BuiltinHookRegistry`) at
/// hydrate time. `insertion_idx` is not exposed — the DB manages it
/// internally so load order within a phase matches insertion order.
#[derive(Debug, Clone)]
pub struct HookRow {
    pub hook_id: String,
    pub phase: String,
    pub priority: i32,
    pub match_instance: Option<String>,
    pub match_tool: Option<String>,
    pub match_context: Option<ContextId>,
    pub match_principal: Option<PrincipalId>,
    pub action_kind: String,
    pub action_builtin_name: Option<String>,
    /// Snapshot of the kaish body that fires at evaluation. Always
    /// set when `action_kind = "kaish_invoke"`. Even hooks installed
    /// via a shared `hook_scripts` reference snapshot the body here
    /// at install time per
    /// `feedback_script_snapshot_on_instantiation`.
    pub action_kaish_body: Option<String>,
    /// Provenance: if installed from a shared script, the originating
    /// `script_id`. Not re-resolved at hydrate; metadata only.
    pub action_kaish_script_id: Option<String>,
    pub action_result_text: Option<String>,
    pub action_is_error: Option<bool>,
    pub action_deny_reason: Option<String>,
    pub action_log_target: Option<String>,
    pub action_log_level: Option<String>,
}

/// A shared kaish script body, referenced by zero or more hooks via
/// `hooks.action_kaish_script_id`. DB-global (matches the `hooks`
/// table); `script_id` is caller-supplied (defaults to UUIDv4 at the
/// admin surface) so common scripts can be addressed by stable name.
#[derive(Debug, Clone)]
pub struct HookScriptRow {
    pub script_id: String,
    pub body: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub updated_at: i64,
}

/// A preset template row.
#[derive(Debug, Clone)]
pub struct PresetRow {
    pub preset_id: PresetId,
    pub label: String,
    pub description: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub consent_mode: ConsentMode,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// One normalized, verb-scoped preset argument (a row of `preset_args`). The
/// "filter knobs" a patch recalls — e.g. `("include", "0:5")` under verb
/// `"fork"`. Repeatable arg names carry multiple `PresetArg`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetArg {
    pub arg_name: String,
    pub arg_value: String,
}

/// A workspace row.
#[derive(Debug, Clone)]
pub struct WorkspaceRow {
    pub workspace_id: WorkspaceId,
    pub label: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
    pub archived_at: Option<i64>,
}

/// A workspace path row.
#[derive(Debug, Clone)]
pub struct WorkspacePathRow {
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub read_only: bool,
    pub created_at: i64,
}

/// A document row — CRDT content layer.
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub document_id: ContextId,
    pub workspace_id: WorkspaceId,
    pub doc_kind: DocKind,
    pub language: Option<String>,
    pub path: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// A doc_snapshots row — compaction checkpoint.
#[derive(Debug, Clone)]
pub struct DocSnapshotRow {
    pub document_id: ContextId,
    pub seq: i64,
    pub version: i64,
    pub state: Vec<u8>,
    pub content: String,
    pub created_at: i64,
}

/// An input_doc_snapshots row — compaction checkpoint for input docs.
#[derive(Debug, Clone)]
pub struct InputDocSnapshotRow {
    pub document_id: ContextId,
    pub seq: i64,
    pub state: Vec<u8>,
    pub content: String,
    pub created_at: i64,
}

/// Per-context shell configuration.
#[derive(Debug, Clone)]
pub struct ContextShellRow {
    pub context_id: ContextId,
    pub cwd: Option<String>,
    pub updated_at: i64,
}

/// Per-context environment variable.
#[derive(Debug, Clone)]
pub struct ContextEnvRow {
    pub context_id: ContextId,
    pub key: String,
    pub value: String,
}

/// A context edge row (structural or drift).
#[derive(Debug, Clone)]
pub struct ContextEdgeRow {
    pub edge_id: uuid::Uuid,
    pub source_id: ContextId,
    pub target_id: ContextId,
    pub kind: EdgeKind,
    pub metadata: Option<String>,
    pub created_at: i64,
}

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
CREATE TABLE IF NOT EXISTS presets (
    preset_id    BLOB NOT NULL PRIMARY KEY,
    label        TEXT NOT NULL UNIQUE,
    description  TEXT,
    provider     TEXT,
    model        TEXT,
    system_prompt TEXT,
    consent_mode TEXT NOT NULL DEFAULT 'collaborative',
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

-- ── Documents (CRDT content layer) ─────────────────────────────
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
    provider     TEXT,
    model        TEXT,
    system_prompt TEXT,
    consent_mode TEXT NOT NULL DEFAULT 'collaborative',
    context_state TEXT NOT NULL DEFAULT 'live',
    context_type TEXT NOT NULL DEFAULT 'default',
    created_at   INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    created_by   BLOB NOT NULL,
    forked_from  BLOB REFERENCES contexts(context_id) ON DELETE SET NULL,
    fork_kind    TEXT,
    archived_at  INTEGER,
    concluded_at INTEGER,
    workspace_id BLOB REFERENCES workspaces(workspace_id) ON DELETE SET NULL,
    preset_id    BLOB REFERENCES presets(preset_id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_contexts_label
    ON contexts(label) WHERE label IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_contexts_workspace
    ON contexts(workspace_id) WHERE workspace_id IS NOT NULL;

-- ── Context Edges ───────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS context_edges (
    edge_id    BLOB NOT NULL PRIMARY KEY,
    source_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    target_id  BLOB NOT NULL REFERENCES contexts(context_id) ON DELETE CASCADE,
    kind       TEXT NOT NULL DEFAULT 'structural',
    metadata   TEXT,
    created_at INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_edges_structural_unique
    ON context_edges(source_id, target_id) WHERE kind = 'structural';
CREATE INDEX IF NOT EXISTS idx_edges_source ON context_edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON context_edges(target_id);

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

-- Compaction checkpoints: latest snapshot per document.
CREATE TABLE IF NOT EXISTS doc_snapshots (
    document_id BLOB    NOT NULL PRIMARY KEY,
    seq         INTEGER NOT NULL,
    version     INTEGER NOT NULL,
    state       BLOB    NOT NULL,
    content     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
);

-- ── Input Document Op-Log ───────────────────────────────────────
CREATE TABLE IF NOT EXISTS input_oplog (
    document_id BLOB    NOT NULL,
    seq         INTEGER NOT NULL,
    payload     BLOB    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    PRIMARY KEY (document_id, seq),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS input_doc_snapshots (
    document_id BLOB    NOT NULL PRIMARY KEY,
    seq         INTEGER NOT NULL,
    state       BLOB    NOT NULL,
    content     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER)),
    FOREIGN KEY (document_id) REFERENCES documents(document_id) ON DELETE CASCADE
);

-- ── Context Shell / Env ─────────────────────────────────────────
CREATE TABLE IF NOT EXISTS context_shell (
    context_id  BLOB NOT NULL PRIMARY KEY REFERENCES contexts(context_id) ON DELETE CASCADE,
    cwd         TEXT,
    updated_at  INTEGER NOT NULL DEFAULT (CAST((unixepoch('subsec') * 1000) AS INTEGER))
);

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
    binding_rc_write INTEGER NOT NULL DEFAULT 0, -- "rc-write" — may write /etc/rc via file tools
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
-- implied by all_instances, mirroring binding_admin / binding_rc_write.
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
    action_result_text       TEXT,
    action_is_error          INTEGER,
    action_deny_reason       TEXT,
    action_log_target        TEXT,
    action_log_level         TEXT,
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
-- /etc/rc (~/.config/kaijutsu/rc), seeded to disk at boot. See
-- crate::seed_scripts and kj/lifecycle.rs. A legacy `rc_scripts` table may
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
    playing           INTEGER NOT NULL DEFAULT 0   -- 1=Playing, 0=Stopped
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

/// A persisted track clock domain — the restart-recovery row for a `TrackState`.
/// Primitive-typed so `kernel_db` stays decoupled from `hyoushigi`'s runtime
/// types; the beat/scheduler layer converts at the seam. (`period_ms` ↔
/// `Duration`, `track_id` ↔ `TrackId`, `playing` ↔ `TrackTransport`.)
///
/// Cold-start always re-arms stopped (`playing` restored from this row) so the
/// scheduler never fires stale wakeups on a warm start — the caller calls
/// `play` explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTrack {
    pub track_id: String,
    pub period_ms: u64,
    pub beats_per_phrase: u64,
    /// The track's musical position (playhead tick) as of the last durable update.
    /// `None` until the track first beats; thereafter the max committed tick so
    /// a restart can reseed without scanning the block log.
    pub playhead_tick: Option<i64>,
    /// Transport state: `true` = Playing, `false` = Stopped.
    pub playing: bool,
}

/// A persisted attachment — one (track_id, context_id) binding, the restart-
/// recovery row for a `TrackState.attached[ctx]` entry. Primitive-typed for the
/// same reason as `PersistedTrack`; the scheduler converts at the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAttachment {
    pub track_id: String,
    pub context_id: ContextId,
    /// Beat divisor: wake this context every N beats on the track clock.
    pub wakeup_every: u64,
    /// Self-fork rotate cadence in phrases; `None` = never auto-rotate.
    pub rotate_every_phrases: Option<u64>,
    /// Whether the OODA loop is armed for this attachment (blocks the next tick
    /// until the context produces output).
    pub ooda_armed: bool,
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

/// Wire a [`CacheTarget`] onto the three storage columns:
/// `(target_kind, target_index, ttl)`. `target_index` is non-null only
/// for `MessageIndex`. `ttl` is the lowercase variant name.
fn encode_cache_target(target: &CacheTarget) -> (&'static str, Option<i64>, &'static str) {
    let (kind, index) = match target {
        CacheTarget::Tools(_) => ("tools", None),
        CacheTarget::System(_) => ("system", None),
        CacheTarget::MessageIndex(i, _) => ("message_index", Some(*i as i64)),
    };
    let ttl = match target.ttl() {
        CacheTtl::Ephemeral => "ephemeral",
        CacheTtl::Extended => "extended",
    };
    (kind, index, ttl)
}

/// Reverse of [`encode_cache_target`]. Returns `None` when the row carries
/// an unrecognized `target_kind`, missing `target_index` for
/// `message_index`, or an unrecognized `ttl` — the caller logs and skips.
fn decode_cache_target(kind: &str, index: Option<i64>, ttl: &str) -> Option<CacheTarget> {
    let ttl = match ttl {
        "ephemeral" => CacheTtl::Ephemeral,
        "extended" => CacheTtl::Extended,
        _ => return None,
    };
    match kind {
        "tools" => Some(CacheTarget::Tools(ttl)),
        "system" => Some(CacheTarget::System(ttl)),
        "message_index" => {
            let i = index?;
            if i < 0 {
                return None;
            }
            Some(CacheTarget::MessageIndex(i as usize, ttl))
        }
        _ => None,
    }
}

/// Parse ConsentMode from TEXT column.
fn consent_mode_from_sql(s: &str) -> ConsentMode {
    ConsentMode::from_str(s).unwrap_or_else(|_| {
        warn!(mode = %s, "unknown ConsentMode in DB, defaulting to Collaborative");
        ConsentMode::Collaborative
    })
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

/// Parse EdgeKind from TEXT column.
fn edge_kind_from_sql(s: &str) -> EdgeKind {
    EdgeKind::from_str(s).unwrap_or_else(|_| {
        warn!(kind = %s, "unknown EdgeKind in DB, defaulting to Structural");
        EdgeKind::Structural
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

// ============================================================================
// KernelDb
// ============================================================================

/// SQLite database for kernel context metadata.
pub struct KernelDb {
    conn: Connection,
}

impl KernelDb {
    fn init_connection(conn: &Connection) -> SqliteResult<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(())
    }

    /// Additive column backfills for DBs created before a column existed.
    /// The project's stance is "schema is truth, bump = wipe", but a single
    /// `ADD COLUMN ... DEFAULT 0` is cheap and spares a live kernel a wipe.
    /// Each ALTER is guarded: a "duplicate column" error on a fresh DB (the
    /// column is already in `SCHEMA`) is expected and ignored.
    fn apply_additive_migrations(conn: &Connection) {
        let alters = [
            "ALTER TABLE context_bindings ADD COLUMN binding_rc_write INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE contexts ADD COLUMN concluded_at INTEGER",
        ];
        for sql in alters {
            // Ignore "duplicate column name" (column already present); a real
            // failure surfaces on the next read of the column.
            let _ = conn.execute(sql, []);
        }
    }

    /// Open or create at the given path.
    ///
    /// The DB schema is the single source of truth — there are no
    /// migrations. Bumping the schema requires wiping the DB. rc lifecycle
    /// scripts are no longer table rows — they live as files under
    /// `/etc/rc` (see `seed_scripts` and `kj/lifecycle.rs`), seeded to disk
    /// at server boot.
    pub fn open<P: AsRef<Path>>(path: P) -> KernelDbResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init_connection(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Self::apply_additive_migrations(&conn);
        Self::ensure_singleton_kernel(&conn)?;
        Ok(Self { conn })
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> KernelDbResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_connection(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Self::apply_additive_migrations(&conn);
        Self::ensure_singleton_kernel(&conn)?;
        Ok(Self { conn })
    }

    /// Read any legacy `rc_scripts` rows from a pre-files DB, as
    /// `(canonical_path, content)` pairs. Used once at boot to migrate a
    /// user's customizations onto the `/etc/rc` file tree. New DBs never
    /// create the table, so a missing table yields an empty vec (not an
    /// error) — the migration simply no-ops.
    pub fn legacy_rc_scripts(&self) -> Vec<(String, String)> {
        let mut stmt = match self
            .conn
            .prepare("SELECT path, content FROM rc_scripts")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(), // table absent on new DBs
        };
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    // ========================================================================
    // Kernel identity
    // ========================================================================

    /// Create the singleton kernel row on first open. Subsequent opens
    /// are no-ops because the row's `singleton` value is fixed at 1 and
    /// `INSERT OR IGNORE` short-circuits on the PK conflict.
    fn ensure_singleton_kernel(conn: &Connection) -> KernelDbResult<()> {
        let id = KernelId::new();
        let founder = PrincipalId::system();
        conn.execute(
            "INSERT OR IGNORE INTO kernel (singleton, id, founder, label)
             VALUES (1, ?1, ?2, NULL)",
            params![blob_param(id.as_bytes()), blob_param(founder.as_bytes())],
        )?;
        Ok(())
    }

    /// The stable kernel ID for this database — written on first open,
    /// immutable thereafter. Used by the wire `bind_kernel` / `ping`
    /// handshake so clients can detect when they're talking to a
    /// different kernel than the one they bound to.
    pub fn kernel_id(&self) -> KernelDbResult<KernelId> {
        let bytes: Vec<u8> = self
            .conn
            .query_row("SELECT id FROM kernel WHERE singleton = 1", [], |row| {
                row.get(0)
            })?;
        KernelId::try_from_slice(&bytes).ok_or_else(|| {
            KernelDbError::Validation("kernel row holds invalid id bytes".to_string())
        })
    }

    /// Set or clear the kernel's human-friendly label.
    pub fn set_kernel_label(&self, label: Option<&str>) -> KernelDbResult<()> {
        if let Some(l) = label {
            validate_label(l)?;
        }
        self.conn.execute(
            "UPDATE kernel SET label = ?1 WHERE singleton = 1",
            params![label],
        )?;
        Ok(())
    }

    // ========================================================================
    // Auto-Workspaces
    // ========================================================================

    /// Get or create the `__system` workspace (for config docs).
    /// Returns the workspace ID.
    pub fn get_or_create_system_workspace(
        &self,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        self.get_or_create_builtin_workspace(
            "__system",
            "System configuration documents",
            created_by,
        )
    }

    /// Get or create the `__default` workspace (for conversations and file cache).
    /// Returns the workspace ID.
    pub fn get_or_create_default_workspace(
        &self,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        self.get_or_create_builtin_workspace("__default", "Default workspace", created_by)
    }

    /// Get or create a built-in workspace by label.
    fn get_or_create_builtin_workspace(
        &self,
        label: &str,
        description: &str,
        created_by: PrincipalId,
    ) -> KernelDbResult<WorkspaceId> {
        if let Some(ws) = self.get_workspace_by_label(label)? {
            return Ok(ws.workspace_id);
        }
        let ws_id = WorkspaceId::new();
        let row = WorkspaceRow {
            workspace_id: ws_id,
            label: label.to_string(),
            description: Some(description.to_string()),
            created_at: now_millis(),
            created_by,
            archived_at: None,
        };
        self.insert_workspace(&row)?;
        info!(workspace_id = %ws_id.to_hex(), label, "Created built-in workspace");
        Ok(ws_id)
    }

    // ========================================================================
    // Document CRUD
    // ========================================================================

    /// Insert a new document.
    pub fn insert_document(&self, row: &DocumentRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO documents (
                document_id, workspace_id, doc_kind,
                language, path, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    blob_param(row.document_id.as_bytes()),
                    blob_param(row.workspace_id.as_bytes()),
                    row.doc_kind.as_str(),
                    row.language,
                    row.path,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                ],
            )
            .map_err(|e| map_unique_violation(e, "document already exists or path conflict"))?;
        Ok(())
    }

    /// Insert a document, ignoring if it already exists (idempotent).
    pub fn insert_document_or_ignore(&self, row: &DocumentRow) -> KernelDbResult<()> {
        Self::write_document_or_ignore(&self.conn, row)
    }

    /// Insert-or-ignore a document row against `conn` (a `&Connection` or a
    /// `&Transaction` via deref). Shared by `insert_document_or_ignore` and
    /// the transactional `insert_forked_context`. Does NOT commit.
    fn write_document_or_ignore(conn: &Connection, row: &DocumentRow) -> KernelDbResult<()> {
        conn.execute(
            "INSERT OR IGNORE INTO documents (
                document_id, workspace_id, doc_kind,
                language, path, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob_param(row.document_id.as_bytes()),
                blob_param(row.workspace_id.as_bytes()),
                row.doc_kind.as_str(),
                row.language,
                row.path,
                row.created_at,
                blob_param(row.created_by.as_bytes()),
            ],
        )?;
        Ok(())
    }

    /// Get a document by ID.
    pub fn get_document(&self, id: ContextId) -> KernelDbResult<Option<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_document_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all documents in this kernel.
    pub fn list_documents(&self) -> KernelDbResult<Vec<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |row| row_to_document_row(row))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List documents filtered by kind.
    pub fn list_documents_by_kind(&self, kind: DocKind) -> KernelDbResult<Vec<DocumentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE doc_kind = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![kind.as_str()], row_to_document_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List documents whose `path` falls strictly *under* `dir` (i.e. matches
    /// `<dir>/...`), ordered by path. This is the prefix-scan that backs
    /// `readdir` for the CRDT-native config/rc backend: the `documents` table
    /// *is* the path manifest (every path-carrying doc is one entry). `dir`
    /// must not end in `/`; the exact `dir` row itself is excluded.
    pub fn list_documents_under_path(&self, dir: &str) -> KernelDbResult<Vec<DocumentRow>> {
        // SQLite LIKE: rc/config paths contain no `%`/`_`, so no ESCAPE needed.
        let pattern = format!("{dir}/%");
        let mut stmt = self.conn.prepare(
            "SELECT document_id, workspace_id, doc_kind,
                    language, path, created_at, created_by
             FROM documents WHERE path LIKE ?1
             ORDER BY path",
        )?;
        let rows = stmt.query_map(params![pattern], row_to_document_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a document (CASCADE deletes snapshots, input_docs, and context).
    pub fn delete_document(&self, id: ContextId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM documents WHERE document_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Op-Log Persistence
    // ========================================================================

    /// Append an op to the journal for a document.
    pub fn append_op(
        &self,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(document_id.as_bytes()), seq, payload],
        )?;
        Ok(())
    }

    /// Load oplog entries after a given seq (for replay after snapshot restore).
    pub fn load_oplog_since(
        &self,
        document_id: ContextId,
        after_seq: i64,
    ) -> KernelDbResult<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, payload FROM oplog
             WHERE document_id = ?1 AND seq > ?2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            params![blob_param(document_id.as_bytes()), after_seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Load the latest compaction snapshot for a document.
    pub fn load_latest_snapshot(
        &self,
        document_id: ContextId,
    ) -> KernelDbResult<Option<DocSnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, seq, version, state, content, created_at
             FROM doc_snapshots WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(document_id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(DocSnapshotRow {
                document_id: read_context_id(row, 0)?,
                seq: row.get(1)?,
                version: row.get(2)?,
                state: row.get(3)?,
                content: row.get(4)?,
                created_at: row.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Write a compaction snapshot and truncate the oplog up to that seq.
    /// Must be called with exclusive access (the Mutex guarantees this).
    pub fn write_snapshot_and_truncate(
        &mut self,
        document_id: ContextId,
        seq: i64,
        version: i64,
        state: &[u8],
        content: &str,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO doc_snapshots (document_id, seq, version, state, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob_param(document_id.as_bytes()),
                seq,
                version,
                state,
                content,
            ],
        )?;
        tx.execute(
            "DELETE FROM oplog WHERE document_id = ?1 AND seq <= ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        tx.commit()?;
        Ok(())
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

    // ========================================================================
    // Input Document Op-Log
    // ========================================================================

    /// Append an input doc op to the journal.
    pub fn append_input_op(
        &self,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO input_oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(document_id.as_bytes()), seq, payload],
        )?;
        Ok(())
    }

    /// Load input oplog entries after a given seq.
    pub fn load_input_oplog_since(
        &self,
        document_id: ContextId,
        after_seq: i64,
    ) -> KernelDbResult<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, payload FROM input_oplog
             WHERE document_id = ?1 AND seq > ?2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            params![blob_param(document_id.as_bytes()), after_seq],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Load the latest input doc compaction snapshot.
    pub fn load_latest_input_snapshot(
        &self,
        document_id: ContextId,
    ) -> KernelDbResult<Option<InputDocSnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, seq, state, content, created_at
             FROM input_doc_snapshots WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(document_id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(InputDocSnapshotRow {
                document_id: read_context_id(row, 0)?,
                seq: row.get(1)?,
                state: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Write an input doc compaction snapshot and truncate its oplog.
    pub fn write_input_snapshot_and_truncate(
        &mut self,
        document_id: ContextId,
        seq: i64,
        state: &[u8],
        content: &str,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO input_doc_snapshots (document_id, seq, state, content)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                blob_param(document_id.as_bytes()),
                seq,
                state,
                content,
            ],
        )?;
        tx.execute(
            "DELETE FROM input_oplog WHERE document_id = ?1 AND seq <= ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// List document IDs that have input oplog entries or snapshots.
    pub fn list_input_doc_ids(&self) -> KernelDbResult<Vec<ContextId>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT d.document_id FROM documents d
             WHERE EXISTS (SELECT 1 FROM input_oplog o WHERE o.document_id = d.document_id)
                OR EXISTS (SELECT 1 FROM input_doc_snapshots s WHERE s.document_id = d.document_id)",
        )?;
        let rows = stmt.query_map([], |row| read_context_id(row, 0))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ========================================================================
    // Context CRUD
    // ========================================================================

    /// Insert a document + context pair in one call (convenience for conversations).
    ///
    /// Creates the document row first, then the context row. Uses the
    /// default workspace if no workspace_id is set on the context.
    pub fn insert_context_with_document(
        &self,
        row: &ContextRow,
        default_workspace_id: WorkspaceId,
    ) -> KernelDbResult<()> {
        let ws_id = row.workspace_id.unwrap_or(default_workspace_id);
        self.insert_document_or_ignore(&DocumentRow {
            document_id: row.context_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row.created_at,
            created_by: row.created_by,
        })?;
        self.insert_context(row)
    }

    /// Atomically create a forked context: the document row, the context row,
    /// and the shell + env + capability-binding config copied from `source`,
    /// all in ONE transaction.
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
    ) -> KernelDbResult<()> {
        let shell = self.get_context_shell(source)?;
        let env = self.get_context_env(source)?;
        let binding = self.get_context_binding(source)?;

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
        if let Some(src) = shell {
            Self::write_context_shell(
                &tx,
                &ContextShellRow {
                    context_id: row.context_id,
                    cwd: src.cwd,
                    updated_at: now_millis(),
                },
            )?;
        }
        for var in &env {
            Self::write_context_env(&tx, row.context_id, &var.key, &var.value)?;
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
        tx.commit()?;
        Ok(())
    }

    /// Insert a new context.
    ///
    /// The corresponding document row must already exist (FK enforced).
    pub fn insert_context(&self, row: &ContextRow) -> KernelDbResult<()> {
        Self::write_context(&self.conn, row)
    }

    /// Insert a context row against `conn` (a `&Connection` or a
    /// `&Transaction` via deref). Shared by `insert_context` and the
    /// transactional `insert_forked_context`. Does NOT commit.
    fn write_context(conn: &Connection, row: &ContextRow) -> KernelDbResult<()> {
        if let Some(ref label) = row.label {
            validate_label(label)?;
        }

        conn.execute(
                "INSERT INTO contexts (
                context_id, label, provider, model,
                system_prompt, consent_mode, context_state, context_type,
                created_at, created_by, forked_from, fork_kind,
                archived_at, workspace_id, preset_id, concluded_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8, ?9,
                ?10, ?11, ?12,
                ?13, ?14, ?15, ?16
            )",
                params![
                    blob_param(row.context_id.as_bytes()),
                    row.label,
                    row.provider,
                    row.model,
                    row.system_prompt,
                    row.consent_mode.as_str(),
                    row.context_state.as_str(),
                    row.context_type,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.forked_from.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.fork_kind.map(|fk| fk.as_str().to_string()),
                    row.archived_at,
                    row.workspace_id.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.preset_id.as_ref().map(|id| id.as_bytes().to_vec()),
                    row.concluded_at,
                ],
            )
            .map_err(|e| {
                if let Some(ref label) = row.label {
                    map_unique_violation(e, format!("label '{}' already in use", label))
                } else {
                    KernelDbError::Db(e)
                }
            })?;
        Ok(())
    }

    /// Get a context by ID.
    pub fn get_context(&self, id: ContextId) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label, provider, model,
                    system_prompt, consent_mode, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at
             FROM contexts WHERE context_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Update a context's label.
    pub fn update_label(&self, id: ContextId, label: Option<&str>) -> KernelDbResult<()> {
        if let Some(l) = label {
            validate_label(l)?;
        }

        let updated = self
            .conn
            .execute(
                "UPDATE contexts SET label = ?1 WHERE context_id = ?2",
                params![label, blob_param(id.as_bytes())],
            )
            .map_err(|e| {
                if let Some(l) = label {
                    map_unique_violation(e, format!("label '{}' already in use", l))
                } else {
                    KernelDbError::Db(e)
                }
            })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's model assignment.
    pub fn update_model(
        &self,
        id: ContextId,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET provider = ?1, model = ?2 WHERE context_id = ?3",
            params![provider, model, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's settings.
    pub fn update_settings(
        &self,
        id: ContextId,
        system_prompt: Option<&str>,
        consent_mode: ConsentMode,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET system_prompt = ?1, consent_mode = ?2
             WHERE context_id = ?3",
            params![
                system_prompt,
                consent_mode.as_str(),
                blob_param(id.as_bytes()),
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's `context_type` — selects which rc lifecycle
    /// scripts run for this context's create / fork / attach / drift
    /// moments.
    pub fn update_context_type(&self, id: ContextId, context_type: &str) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET context_type = ?1 WHERE context_id = ?2",
            params![context_type, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Update a context's workspace assignment.
    pub fn update_workspace(
        &self,
        id: ContextId,
        ws_id: Option<WorkspaceId>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET workspace_id = ?1 WHERE context_id = ?2",
            params![
                ws_id.as_ref().map(|id| id.as_bytes().to_vec()),
                blob_param(id.as_bytes()),
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("context {}", id.short())));
        }
        Ok(())
    }

    /// Archive a context (soft delete). Returns true if it was active.
    pub fn archive_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts SET archived_at = ?1
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Update the lifecycle state of a context.
    pub fn update_context_state(
        &self,
        id: ContextId,
        state: ContextState,
    ) -> KernelDbResult<bool> {
        let updated = self.conn.execute(
            "UPDATE contexts SET context_state = ?1 WHERE context_id = ?2",
            params![state.as_str(), blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Conclude a context: set `context_state = 'concluded'` and stamp
    /// `concluded_at` (first time only — idempotent re-conclude keeps the
    /// original timestamp). Only an active (non-archived) context can be
    /// concluded. Returns `true` if a row was newly concluded, `false` if the
    /// context was unknown, archived, or already concluded.
    pub fn conclude_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts
                SET context_state = 'concluded', concluded_at = ?1
             WHERE context_id = ?2
               AND archived_at IS NULL
               AND concluded_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// List active (non-archived) contexts.
    pub fn list_active_contexts(&self) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label, provider, model,
                    system_prompt, consent_mode, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at
             FROM contexts
             WHERE archived_at IS NULL
             ORDER BY created_at",
        )?;

        let rows = stmt.query_map([], |row| row_to_context_row(row))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all contexts (including archived).
    pub fn list_all_contexts(&self) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label, provider, model,
                    system_prompt, consent_mode, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at
             FROM contexts
             ORDER BY created_at",
        )?;

        let rows = stmt.query_map([], |row| row_to_context_row(row))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Resolve a context query string.
    ///
    /// Supports exact label, label prefix, hex prefix. For tag:prefix syntax
    /// (future), walks lineage via CTE.
    pub fn resolve_context(&self, query: &str) -> KernelDbResult<ContextId> {
        // Load active contexts (set is small, <100)
        let contexts = self.list_active_contexts()?;
        let items = contexts.iter().map(|c| (c.context_id, c.label.as_deref()));

        kaijutsu_types::resolve_prefix(items, query).map_err(|e| match e {
            kaijutsu_types::PrefixError::NoMatch(q) => {
                KernelDbError::NotFound(format!("no context matches '{}'", q))
            }
            kaijutsu_types::PrefixError::Ambiguous { prefix, candidates } => {
                KernelDbError::Validation(format!(
                    "ambiguous prefix '{}': matches {:?}",
                    prefix, candidates
                ))
            }
        })
    }

    /// Get structural parents of a context.
    pub fn structural_parents(&self, id: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.context_id, c.label, c.provider, c.model,
                    c.system_prompt, c.consent_mode, c.context_state, c.context_type,
                    c.created_at, c.created_by, c.forked_from, c.fork_kind,
                    c.archived_at, c.workspace_id, c.preset_id, c.concluded_at
             FROM contexts c
             JOIN context_edges e ON e.source_id = c.context_id
             WHERE e.target_id = ?1 AND e.kind = 'structural'",
        )?;

        let rows = stmt.query_map(params![blob_param(id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Get active structural children of a context.
    pub fn structural_children(&self, id: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.context_id, c.label, c.provider, c.model,
                    c.system_prompt, c.consent_mode, c.context_state, c.context_type,
                    c.created_at, c.created_by, c.forked_from, c.fork_kind,
                    c.archived_at, c.workspace_id, c.preset_id, c.concluded_at
             FROM contexts c
             JOIN context_edges e ON e.target_id = c.context_id
             WHERE e.source_id = ?1 AND e.kind = 'structural'
               AND c.archived_at IS NULL",
        )?;

        let rows = stmt.query_map(params![blob_param(id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Walk the full context DAG via recursive CTE on structural edges.
    /// Returns `(ContextRow, depth)` pairs.
    pub fn context_dag(&self) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        // Find roots: contexts with no incoming structural edges.
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE dag(ctx_id, depth) AS (
                -- Roots: contexts with no incoming structural edges
                SELECT c.context_id, 0
                FROM contexts c
                WHERE c.archived_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM context_edges e
                      WHERE e.target_id = c.context_id AND e.kind = 'structural'
                  )
                UNION ALL
                -- Children via structural edges
                SELECT e.target_id, dag.depth + 1
                FROM dag
                JOIN context_edges e ON e.source_id = dag.ctx_id AND e.kind = 'structural'
                JOIN contexts c2 ON c2.context_id = e.target_id AND c2.archived_at IS NULL
            )
            SELECT c.context_id, c.label, c.provider, c.model,
                   c.system_prompt, c.consent_mode, c.context_state, c.context_type,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                   dag.depth
            FROM dag
            JOIN contexts c ON c.context_id = dag.ctx_id
            ORDER BY dag.depth, c.created_at",
        )?;

        let rows = stmt.query_map([], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(16)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Walk fork lineage from a context upward via `forked_from`.
    /// Returns `(ContextRow, depth)` where depth 0 = the starting context.
    pub fn fork_lineage(&self, context_id: ContextId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE lineage(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT c.forked_from, lineage.depth + 1
                FROM lineage
                JOIN contexts c ON c.context_id = lineage.ctx_id
                WHERE c.forked_from IS NOT NULL
            )
            SELECT c.context_id, c.label, c.provider, c.model,
                   c.system_prompt, c.consent_mode, c.context_state, c.context_type,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                   lineage.depth
            FROM lineage
            JOIN contexts c ON c.context_id = lineage.ctx_id
            ORDER BY lineage.depth",
        )?;

        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(16)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Snapshot a subtree rooted at `root_id` via structural edges.
    /// Returns `(ContextRow, depth)`.
    pub fn subtree_snapshot(&self, root_id: ContextId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE subtree(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT e.target_id, subtree.depth + 1
                FROM subtree
                JOIN context_edges e ON e.source_id = subtree.ctx_id AND e.kind = 'structural'
            )
            SELECT c.context_id, c.label, c.provider, c.model,
                   c.system_prompt, c.consent_mode, c.context_state, c.context_type,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                   subtree.depth
            FROM subtree
            JOIN contexts c ON c.context_id = subtree.ctx_id
            ORDER BY subtree.depth, c.created_at",
        )?;

        let rows = stmt.query_map(params![blob_param(root_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(16)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ========================================================================
    // Context Edges
    // ========================================================================

    /// Insert an edge. Structural edges get cycle detection.
    pub fn insert_edge(&self, row: &ContextEdgeRow) -> KernelDbResult<()> {
        if row.kind == EdgeKind::Structural {
            // Check for cycle: walk descendants of target — if source appears, reject.
            if self.would_create_cycle(row.source_id, row.target_id)? {
                return Err(KernelDbError::CycleDetected);
            }
        }

        self.conn.execute(
            "INSERT INTO context_edges (edge_id, source_id, target_id, kind, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.edge_id.as_bytes().as_slice(),
                blob_param(row.source_id.as_bytes()),
                blob_param(row.target_id.as_bytes()),
                row.kind.as_str(),
                row.metadata,
                row.created_at,
            ],
        ).map_err(|e| {
            map_unique_violation(e, format!(
                "structural edge {:?} → {:?} already exists",
                row.source_id.short(), row.target_id.short()
            ))
        })?;
        Ok(())
    }

    /// Check if adding source→target structural edge would create a cycle.
    fn would_create_cycle(&self, source: ContextId, target: ContextId) -> KernelDbResult<bool> {
        if source == target {
            return Ok(true);
        }

        // Walk descendants of target via structural edges.
        let mut visited = HashSet::new();
        let mut stack = vec![target];

        while let Some(node) = stack.pop() {
            if node == source {
                return Ok(true);
            }
            if !visited.insert(node) {
                continue;
            }

            let mut stmt = self.conn.prepare(
                "SELECT target_id FROM context_edges
                 WHERE source_id = ?1 AND kind = 'structural'",
            )?;
            let children: Vec<ContextId> = stmt
                .query_map(params![blob_param(node.as_bytes())], |row| {
                    read_context_id(row, 0)
                })?
                .collect::<SqliteResult<Vec<_>>>()?;

            stack.extend(children);
        }

        Ok(false)
    }

    /// List edges from a source, optionally filtered by kind.
    pub fn edges_from(
        &self,
        source: ContextId,
        kind: Option<EdgeKind>,
    ) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let rows = match kind {
            Some(k) => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE source_id = ?1 AND kind = ?2
                     ORDER BY created_at",
                )?;
                stmt.query_map(
                    params![blob_param(source.as_bytes()), k.as_str()],
                    row_to_edge_row,
                )?
                .collect::<SqliteResult<Vec<_>>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE source_id = ?1
                     ORDER BY created_at",
                )?;
                stmt.query_map(params![blob_param(source.as_bytes())], row_to_edge_row)?
                    .collect::<SqliteResult<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// List edges to a target, optionally filtered by kind.
    pub fn edges_to(
        &self,
        target: ContextId,
        kind: Option<EdgeKind>,
    ) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let rows = match kind {
            Some(k) => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE target_id = ?1 AND kind = ?2
                     ORDER BY created_at",
                )?;
                stmt.query_map(
                    params![blob_param(target.as_bytes()), k.as_str()],
                    row_to_edge_row,
                )?
                .collect::<SqliteResult<Vec<_>>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT edge_id, source_id, target_id, kind, metadata, created_at
                     FROM context_edges
                     WHERE target_id = ?1
                     ORDER BY created_at",
                )?;
                stmt.query_map(params![blob_param(target.as_bytes())], row_to_edge_row)?
                    .collect::<SqliteResult<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// List drift edges originating from a context, newest first.
    pub fn drift_provenance(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, kind, metadata, created_at
             FROM context_edges
             WHERE source_id = ?1 AND kind = 'drift'
             ORDER BY created_at DESC",
        )?;

        let rows = stmt
            .query_map(params![blob_param(context_id.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    // ========================================================================
    // Presets
    // ========================================================================

    /// Insert a new preset.
    pub fn insert_preset(&self, row: &PresetRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        self.conn
            .execute(
                "INSERT INTO presets (
                preset_id, label, description, provider, model,
                system_prompt, consent_mode, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    blob_param(row.preset_id.as_bytes()),
                    row.label,
                    row.description,
                    row.provider,
                    row.model,
                    row.system_prompt,
                    row.consent_mode.as_str(),
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("preset label '{}' already in use", row.label))
            })?;
        Ok(())
    }

    /// Get a preset by ID.
    pub fn get_preset(&self, id: PresetId) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, label, description, provider, model,
                    system_prompt, consent_mode, created_at, created_by
             FROM presets WHERE preset_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_preset_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a preset by label.
    pub fn get_preset_by_label(&self, label: &str) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, label, description, provider, model,
                    system_prompt, consent_mode, created_at, created_by
             FROM presets WHERE label = ?1",
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_preset_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all presets.
    pub fn list_presets(&self) -> KernelDbResult<Vec<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, label, description, provider, model,
                    system_prompt, consent_mode, created_at, created_by
             FROM presets
             ORDER BY label",
        )?;

        let rows = stmt.query_map([], |row| row_to_preset_row(row))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Update a preset.
    pub fn update_preset(&self, row: &PresetRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        let updated = self
            .conn
            .execute(
                "UPDATE presets SET
                label = ?1, description = ?2, provider = ?3, model = ?4,
                system_prompt = ?5, consent_mode = ?6
             WHERE preset_id = ?7",
                params![
                    row.label,
                    row.description,
                    row.provider,
                    row.model,
                    row.system_prompt,
                    row.consent_mode.as_str(),
                    blob_param(row.preset_id.as_bytes()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("preset label '{}' already in use", row.label))
            })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("preset {}", row.preset_id)));
        }
        Ok(())
    }

    /// Delete a preset. Returns true if it existed. Its `preset_args` rows
    /// cascade-delete with it.
    pub fn delete_preset(&self, id: PresetId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM presets WHERE preset_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    /// Replace all args for `(preset_id, verb)` with `args`, transactionally.
    /// Empty `args` clears the verb's args. Idempotent — duplicate
    /// `(arg_name, arg_value)` pairs collapse via the composite PK.
    pub fn set_preset_args(
        &mut self,
        preset_id: PresetId,
        verb: &str,
        args: &[PresetArg],
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM preset_args WHERE preset_id = ?1 AND verb = ?2",
            params![blob_param(preset_id.as_bytes()), verb],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO preset_args (preset_id, verb, arg_name, arg_value)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for a in args {
                stmt.execute(params![
                    blob_param(preset_id.as_bytes()),
                    verb,
                    a.arg_name,
                    a.arg_value,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Read the args for `(preset_id, verb)`, ordered by `(arg_name,
    /// arg_value)` for a stable round-trip. The selection algebra is order-free,
    /// so value-order is purely for determinism.
    pub fn get_preset_args(&self, preset_id: PresetId, verb: &str) -> KernelDbResult<Vec<PresetArg>> {
        let mut stmt = self.conn.prepare(
            "SELECT arg_name, arg_value FROM preset_args
             WHERE preset_id = ?1 AND verb = ?2
             ORDER BY arg_name, arg_value",
        )?;
        let rows = stmt.query_map(params![blob_param(preset_id.as_bytes()), verb], |row| {
            Ok(PresetArg {
                arg_name: row.get(0)?,
                arg_value: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ========================================================================
    // Workspaces
    // ========================================================================

    /// Insert a new workspace.
    pub fn insert_workspace(&self, row: &WorkspaceRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        self.conn
            .execute(
                "INSERT INTO workspaces (
                workspace_id, label, description, created_at,
                created_by, archived_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    blob_param(row.workspace_id.as_bytes()),
                    row.label,
                    row.description,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.archived_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("workspace label '{}' already in use", row.label))
            })?;
        Ok(())
    }

    /// Get a workspace by ID.
    pub fn get_workspace(&self, id: WorkspaceId) -> KernelDbResult<Option<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces WHERE workspace_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_workspace_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a workspace by label.
    pub fn get_workspace_by_label(&self, label: &str) -> KernelDbResult<Option<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces WHERE label = ?1",
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_workspace_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List active (non-archived) workspaces.
    pub fn list_workspaces(&self) -> KernelDbResult<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces
             WHERE archived_at IS NULL
             ORDER BY label",
        )?;

        let rows = stmt.query_map([], |row| row_to_workspace_row(row))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all workspaces (including archived).
    pub fn list_all_workspaces(&self) -> KernelDbResult<Vec<WorkspaceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, label, description, created_at,
                    created_by, archived_at
             FROM workspaces
             ORDER BY label",
        )?;

        let rows = stmt.query_map([], |row| row_to_workspace_row(row))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Archive a workspace (soft delete). Returns true if it was active.
    pub fn archive_workspace(&self, id: WorkspaceId) -> KernelDbResult<bool> {
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE workspaces SET archived_at = ?1
             WHERE workspace_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    // ========================================================================
    // Workspace Paths
    // ========================================================================

    /// Insert a workspace path.
    pub fn insert_workspace_path(&self, row: &WorkspacePathRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO workspace_paths (workspace_id, path, read_only, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(row.workspace_id.as_bytes()),
                    row.path,
                    row.read_only as i64,
                    row.created_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("workspace path '{}' already exists", row.path))
            })?;
        Ok(())
    }

    /// List paths for a workspace.
    pub fn list_workspace_paths(
        &self,
        workspace_id: WorkspaceId,
    ) -> KernelDbResult<Vec<WorkspacePathRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace_id, path, read_only, created_at
             FROM workspace_paths WHERE workspace_id = ?1
             ORDER BY path",
        )?;

        let rows = stmt.query_map(params![blob_param(workspace_id.as_bytes())], |row| {
            let ro: i64 = row.get(2)?;
            Ok(WorkspacePathRow {
                workspace_id: read_workspace_id(row, 0)?,
                path: row.get(1)?,
                read_only: ro != 0,
                created_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a workspace path. Returns true if it existed.
    pub fn delete_workspace_path(
        &self,
        workspace_id: WorkspaceId,
        path: &str,
    ) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM workspace_paths WHERE workspace_id = ?1 AND path = ?2",
            params![blob_param(workspace_id.as_bytes()), path],
        )?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Context Shell Configuration
    // ========================================================================

    /// Write per-context shell config against `conn` — a bare `Connection`
    /// or an open `Transaction` (which derefs to one). Shared by
    /// `upsert_context_shell` and the transactional `fork_context_config` so
    /// the SQL has a single home.
    fn write_context_shell(conn: &Connection, row: &ContextShellRow) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO context_shell (context_id, cwd, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id) DO UPDATE SET
                cwd = excluded.cwd,
                updated_at = excluded.updated_at",
            params![
                blob_param(row.context_id.as_bytes()),
                row.cwd,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Upsert per-context shell configuration (cwd).
    pub fn upsert_context_shell(&self, row: &ContextShellRow) -> KernelDbResult<()> {
        Self::write_context_shell(&self.conn, row)
    }

    /// Get per-context shell configuration. Returns None if not set.
    pub fn get_context_shell(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<ContextShellRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, cwd, updated_at
             FROM context_shell WHERE context_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok(ContextShellRow {
                context_id: read_context_id(row, 0)?,
                cwd: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Copy shell config from source to target. Returns true if source had config.
    pub fn copy_context_shell(&self, source: ContextId, target: ContextId) -> KernelDbResult<bool> {
        let src = match self.get_context_shell(source)? {
            Some(s) => s,
            None => return Ok(false),
        };
        let row = ContextShellRow {
            context_id: target,
            cwd: src.cwd,
            updated_at: now_millis(),
        };
        self.upsert_context_shell(&row)?;
        Ok(true)
    }

    // ========================================================================
    // Context Tool Bindings (Phase 5, D-54)
    // ========================================================================

    /// Upsert a full `ContextToolBinding` for `context_id`.
    ///
    /// Transactional: bumps the parent row's `updated_at`, then wholesale
    /// replaces the child `context_binding_instances` / `context_binding_names`
    /// rows. Callers treat the binding as the unit of write. This matches the
    /// in-memory model (`Broker::set_binding` is also wholesale) and keeps
    /// restoration trivial (read parent → read children → fold).
    pub fn upsert_context_binding(
        &mut self,
        context_id: ContextId,
        binding: &ContextToolBinding,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        Self::write_binding(&tx, context_id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Wholesale-replace a context's binding (parent flags + every child row)
    /// against `conn`. The caller owns the transaction: `upsert_context_binding`
    /// wraps a fresh one, `fork_context_config` reuses its multi-write tx so
    /// the binding lands atomically alongside the shell + env copy. Does NOT
    /// commit — that is the caller's job.
    fn write_binding(
        conn: &Connection,
        context_id: ContextId,
        binding: &ContextToolBinding,
    ) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO context_bindings
                 (context_id, all_instances, all_facades, binding_admin, binding_rc_write, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(context_id) DO UPDATE SET
                 all_instances    = excluded.all_instances,
                 all_facades      = excluded.all_facades,
                 binding_admin    = excluded.binding_admin,
                 binding_rc_write = excluded.binding_rc_write,
                 updated_at       = excluded.updated_at",
            params![
                blob_param(context_id.as_bytes()),
                binding.all_instances as i64,
                binding.all_facades as i64,
                binding.binding_admin as i64,
                binding.binding_rc_write as i64,
                now_millis(),
            ],
        )?;
        conn.execute(
            "DELETE FROM context_binding_instances WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_names WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_tools WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_facades WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_authorities WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        for (idx, instance) in binding.allowed_instances.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_binding_instances (context_id, instance_id, order_idx)
                 VALUES (?1, ?2, ?3)",
                params![
                    blob_param(context_id.as_bytes()),
                    instance.as_str(),
                    idx as i64,
                ],
            )?;
        }
        for (idx, (instance, tool)) in binding.allowed_tools.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_binding_tools
                     (context_id, instance_id, original_tool, order_idx)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(context_id.as_bytes()),
                    instance.as_str(),
                    tool,
                    idx as i64,
                ],
            )?;
        }
        for (idx, facade) in binding.allowed_facades.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_binding_facades (context_id, facade_id, order_idx)
                 VALUES (?1, ?2, ?3)",
                params![blob_param(context_id.as_bytes()), facade, idx as i64],
            )?;
        }
        // Authorities are a set (BTreeSet iterates sorted) — no order_idx.
        for authority in &binding.authorities {
            conn.execute(
                "INSERT INTO context_binding_authorities (context_id, authority)
                 VALUES (?1, ?2)",
                params![blob_param(context_id.as_bytes()), authority],
            )?;
        }
        for (visible_name, (instance, tool)) in &binding.name_map {
            conn.execute(
                "INSERT INTO context_binding_names
                     (context_id, visible_name, instance_id, original_tool)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(context_id.as_bytes()),
                    visible_name,
                    instance.as_str(),
                    tool,
                ],
            )?;
        }
        Ok(())
    }

    /// Load a `ContextToolBinding` for `context_id`, or `None` if the context
    /// has never been bound. Deny-by-default: callers treat `None` (and an
    /// all-empty binding) as "grants nothing" — there is no first-touch
    /// auto-populate.
    pub fn get_context_binding(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<ContextToolBinding>> {
        let flags: Option<(bool, bool, bool, bool)> = self
            .conn
            .query_row(
                "SELECT all_instances, all_facades, binding_admin, binding_rc_write
                 FROM context_bindings WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .ok();
        let Some((all_instances, all_facades, binding_admin, binding_rc_write)) = flags else {
            return Ok(None);
        };

        let mut allowed_instances: Vec<InstanceId> = {
            let mut stmt = self.conn.prepare(
                "SELECT instance_id FROM context_binding_instances
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let s: String = row.get(0)?;
                Ok(InstanceId::new(s))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_instances.shrink_to_fit();

        let mut allowed_tools: Vec<(InstanceId, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT instance_id, original_tool FROM context_binding_tools
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let inst: String = row.get(0)?;
                let tool: String = row.get(1)?;
                Ok((InstanceId::new(inst), tool))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_tools.shrink_to_fit();

        let mut allowed_facades: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT facade_id FROM context_binding_facades
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                row.get::<_, String>(0)
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_facades.shrink_to_fit();

        let authorities: std::collections::BTreeSet<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT authority FROM context_binding_authorities
                 WHERE context_id = ?1",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                row.get::<_, String>(0)
            })?;
            let mut out = std::collections::BTreeSet::new();
            for r in rows {
                out.insert(r?);
            }
            out
        };

        let mut name_map = std::collections::HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT visible_name, instance_id, original_tool
                 FROM context_binding_names
                 WHERE context_id = ?1",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let visible: String = row.get(0)?;
                let inst: String = row.get(1)?;
                let tool: String = row.get(2)?;
                Ok((visible, (InstanceId::new(inst), tool)))
            })?;
            for r in rows {
                let (visible, pair) = r?;
                name_map.insert(visible, pair);
            }
        }

        Ok(Some(ContextToolBinding {
            all_instances,
            all_facades,
            binding_admin,
            binding_rc_write,
            allowed_instances,
            allowed_tools,
            allowed_facades,
            authorities,
            name_map,
        }))
    }

    /// Delete the full binding for a context (cascades to instances + names).
    /// Returns true if a parent row existed.
    pub fn delete_context_binding(&self, context_id: ContextId) -> KernelDbResult<bool> {
        let rows = self.conn.execute(
            "DELETE FROM context_bindings WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Hook Persistence
    // ========================================================================

    /// Persist one hook entry. `insertion_idx` is computed inside the
    /// INSERT as `MAX(insertion_idx) WHERE phase = ... + 1` so callers
    /// don't have to track it; load order within a phase uses that same
    /// column for tiebreak after `priority`. Fails with the SQLite
    /// UNIQUE-violation if `hook_id` already exists — callers should
    /// `delete_hook` first for replace semantics.
    pub fn insert_hook(&self, row: &HookRow) -> KernelDbResult<()> {
        let is_error: Option<i64> = row.action_is_error.map(|b| if b { 1 } else { 0 });
        let match_context_bytes: Option<Vec<u8>> =
            row.match_context.map(|c| c.as_bytes().to_vec());
        let match_principal_str: Option<String> =
            row.match_principal.map(|p| p.to_string());
        self.conn.execute(
            "INSERT INTO hooks (
                hook_id, phase, priority, insertion_idx,
                match_instance, match_tool, match_context, match_principal,
                action_kind,
                action_builtin_name, action_kaish_body, action_kaish_script_id,
                action_result_text, action_is_error,
                action_deny_reason,
                action_log_target, action_log_level
             ) VALUES (
                ?1, ?2, ?3,
                (SELECT COALESCE(MAX(insertion_idx), -1) + 1 FROM hooks WHERE phase = ?2),
                ?4, ?5, ?6, ?7,
                ?8,
                ?9, ?10, ?11,
                ?12, ?13,
                ?14,
                ?15, ?16
             )",
            params![
                row.hook_id,
                row.phase,
                row.priority,
                row.match_instance,
                row.match_tool,
                match_context_bytes,
                match_principal_str,
                row.action_kind,
                row.action_builtin_name,
                row.action_kaish_body,
                row.action_kaish_script_id,
                row.action_result_text,
                is_error,
                row.action_deny_reason,
                row.action_log_target,
                row.action_log_level,
            ],
        )?;
        Ok(())
    }

    /// Delete a hook by id. Returns true if a row existed.
    pub fn delete_hook(&self, hook_id: &str) -> KernelDbResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM hooks WHERE hook_id = ?1", params![hook_id])?;
        Ok(rows > 0)
    }

    // ========================================================================
    // Hook scripts (kernel-global shared kaish bodies)
    // ========================================================================

    /// Insert a new hook script. Duplicate `script_id` returns
    /// `KernelDbError::AlreadyExists` so callers can distinguish create
    /// vs update.
    pub fn insert_hook_script(&self, row: &HookScriptRow) -> KernelDbResult<()> {
        self.conn
            .execute(
                "INSERT INTO hook_scripts (
                    script_id, body, description,
                    created_at, created_by, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.script_id,
                    row.body,
                    row.description,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                    row.updated_at,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("hook script '{}' already exists", row.script_id))
            })?;
        Ok(())
    }

    /// Replace the body (and optional description) of an existing
    /// script, bumping `updated_at`. Returns `Ok(false)` if no row
    /// exists with the given `script_id`.
    pub fn update_hook_script(
        &self,
        script_id: &str,
        body: &str,
        description: Option<&str>,
    ) -> KernelDbResult<bool> {
        let now = kaijutsu_types::now_millis() as i64;
        let rows = self.conn.execute(
            "UPDATE hook_scripts
             SET body = ?2, description = ?3, updated_at = ?4
             WHERE script_id = ?1",
            params![script_id, body, description, now],
        )?;
        Ok(rows > 0)
    }

    /// Look up a single script by id.
    pub fn get_hook_script(&self, script_id: &str) -> KernelDbResult<Option<HookScriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT script_id, body, description,
                    created_at, created_by, updated_at
             FROM hook_scripts
             WHERE script_id = ?1",
        )?;
        let mut rows = stmt.query(params![script_id])?;
        if let Some(r) = rows.next()? {
            Ok(Some(row_to_hook_script_row(r)?))
        } else {
            Ok(None)
        }
    }

    /// Load every hook script in the DB, ordered by `script_id`.
    /// Used by both the admin surface (`hook_script_list`) and
    /// `Broker::hydrate_hooks_from_db` (to build the script lookup
    /// table for resolving `action_kaish_script_id` references).
    pub fn list_all_hook_scripts(&self) -> KernelDbResult<Vec<HookScriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT script_id, body, description,
                    created_at, created_by, updated_at
             FROM hook_scripts
             ORDER BY script_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_hook_script_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Count hooks referencing this script. Callers use this to enforce
    /// the "fail deletion when referenced" guard rather than cascading.
    pub fn count_hooks_referencing_script(&self, script_id: &str) -> KernelDbResult<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM hooks WHERE action_kaish_script_id = ?1")?;
        let n: i64 = stmt.query_row(params![script_id], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Delete a hook script. Caller must verify
    /// `count_hooks_referencing_script == 0` first to avoid orphaning
    /// `hooks.action_kaish_script_id` references — the schema has no FK
    /// constraint by design (FKs across SQLite ATTACH boundaries hurt;
    /// the guard lives in the application layer).
    pub fn delete_hook_script(&self, script_id: &str) -> KernelDbResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM hook_scripts WHERE script_id = ?1", params![script_id])?;
        Ok(rows > 0)
    }

    /// Load every persisted hook row, ordered by
    /// `(phase ASC, priority ASC, insertion_idx ASC)`. The broker walks
    /// these in order and pushes each onto the matching `HookTable`,
    /// reconstructing the Vec insertion order from before the restart.
    pub fn load_all_hooks(&self) -> KernelDbResult<Vec<HookRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT hook_id, phase, priority,
                    match_instance, match_tool, match_context, match_principal,
                    action_kind,
                    action_builtin_name, action_kaish_body, action_kaish_script_id,
                    action_result_text, action_is_error,
                    action_deny_reason,
                    action_log_target, action_log_level
             FROM hooks
             ORDER BY phase ASC, priority ASC, insertion_idx ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let match_context_bytes: Option<Vec<u8>> = row.get(5)?;
            let match_context = match match_context_bytes {
                Some(bytes) => Some(ContextId::try_from_slice(&bytes).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Blob,
                        "invalid ContextId bytes in hooks.match_context".into(),
                    )
                })?),
                None => None,
            };
            let match_principal_str: Option<String> = row.get(6)?;
            let match_principal = match match_principal_str {
                Some(s) => Some(PrincipalId::parse(&s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        format!("invalid PrincipalId in hooks.match_principal: {e}").into(),
                    )
                })?),
                None => None,
            };
            let is_error_int: Option<i64> = row.get(12)?;
            let action_is_error = is_error_int.map(|i| i != 0);
            Ok(HookRow {
                hook_id: row.get(0)?,
                phase: row.get(1)?,
                priority: row.get(2)?,
                match_instance: row.get(3)?,
                match_tool: row.get(4)?,
                match_context,
                match_principal,
                action_kind: row.get(7)?,
                action_builtin_name: row.get(8)?,
                action_kaish_body: row.get(9)?,
                action_kaish_script_id: row.get(10)?,
                action_result_text: row.get(11)?,
                action_is_error,
                action_deny_reason: row.get(13)?,
                action_log_target: row.get(14)?,
                action_log_level: row.get(15)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ========================================================================
    // Context Environment Variables
    // ========================================================================

    /// Write one env var against `conn` (a `Connection` or an open
    /// `Transaction`). Shared by `set_context_env` and `fork_context_config`.
    fn write_context_env(
        conn: &Connection,
        context_id: ContextId,
        key: &str,
        value: &str,
    ) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO context_env (context_id, key, value)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id, key) DO UPDATE SET value = excluded.value",
            params![blob_param(context_id.as_bytes()), key, value],
        )?;
        Ok(())
    }

    /// Set a single environment variable for a context (upsert).
    pub fn set_context_env(
        &self,
        context_id: ContextId,
        key: &str,
        value: &str,
    ) -> KernelDbResult<()> {
        Self::write_context_env(&self.conn, context_id, key, value)
    }

    /// Get all environment variables for a context.
    pub fn get_context_env(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextEnvRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, key, value FROM context_env
             WHERE context_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok(ContextEnvRow {
                context_id: read_context_id(row, 0)?,
                key: row.get(1)?,
                value: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a single environment variable. Returns true if it existed.
    pub fn delete_context_env(&self, context_id: ContextId, key: &str) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_env WHERE context_id = ?1 AND key = ?2",
            params![blob_param(context_id.as_bytes()), key],
        )?;
        Ok(deleted > 0)
    }

    /// Delete all environment variables for a context. Returns count deleted.
    pub fn clear_context_env(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM context_env WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Copy all env vars from source to target. Returns count copied.
    pub fn copy_context_env(&self, source: ContextId, target: ContextId) -> KernelDbResult<u64> {
        let vars = self.get_context_env(source)?;
        for var in &vars {
            self.set_context_env(target, &var.key, &var.value)?;
        }
        Ok(vars.len() as u64)
    }

    // ========================================================================
    // Claude cache breakpoints (per-context policy)
    // ========================================================================

    /// Append a cache breakpoint for `context_id`. The breakpoint takes the
    /// next available `seq` slot, preserving declaration order across
    /// subsequent reads. Storage is liberal — the 4-cap and dedupe live
    /// in the Claude wire layer (`crate::llm::claude::build::plan_cache`),
    /// not here, so rc populators can over-spec without storage rejecting
    /// them.
    pub fn add_cache_breakpoint(
        &self,
        context_id: ContextId,
        target: &CacheTarget,
    ) -> KernelDbResult<i64> {
        let next_seq: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1
                 FROM cache_breakpoints WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| row.get(0),
            )?;
        let (kind, index, ttl) = encode_cache_target(target);
        self.conn.execute(
            "INSERT INTO cache_breakpoints
                 (context_id, seq, target_kind, target_index, ttl)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob_param(context_id.as_bytes()),
                next_seq,
                kind,
                index,
                ttl,
            ],
        )?;
        Ok(next_seq)
    }

    /// Read all cache breakpoints for `context_id`, ordered by insertion
    /// (`seq` ASC). Rows with an unrecognized `target_kind` or `ttl`
    /// are skipped with a `tracing::warn!` — storage is forwards-compatible
    /// with future variants, but Claude's build layer must not see them.
    pub fn list_cache_breakpoints(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Vec<CacheTarget>> {
        let mut stmt = self.conn.prepare(
            "SELECT target_kind, target_index, ttl
             FROM cache_breakpoints
             WHERE context_id = ?1
             ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![blob_param(context_id.as_bytes())], |row| {
                let kind: String = row.get(0)?;
                let index: Option<i64> = row.get(1)?;
                let ttl: String = row.get(2)?;
                Ok((kind, index, ttl))
            })?
            .collect::<SqliteResult<Vec<_>>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (kind, index, ttl) in rows {
            match decode_cache_target(&kind, index, &ttl) {
                Some(t) => out.push(t),
                None => {
                    warn!(
                        context = %context_id.short(),
                        target_kind = %kind,
                        target_index = ?index,
                        ttl = %ttl,
                        "cache breakpoint row dropped: unrecognized fields"
                    );
                }
            }
        }
        Ok(out)
    }

    /// Delete all cache breakpoints for `context_id`. Returns the row count.
    /// Used by rc-on-drift scripts that want to clear-then-rebuild after a
    /// conversation reshape (compact, model swap, doc inject).
    pub fn clear_cache_breakpoints(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM cache_breakpoints WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Set (upsert) the hydration window policy for `context_id`: from now on the
    /// conversation hydrates only `[0, marker] ∪ last-window_size`, not its whole
    /// history. `marker` is the pinned-prefix end P (a durable block — `[0,P]`
    /// always hydrates and stays cache-stable); `window_size` is the sliding tail
    /// W. The cost guard for endless musician logs (design: `docs/chameleon.md`,
    /// the hydration marker). Upserted at musician-create and on a durable
    /// revision (marker advance) — NOT per turn; the tail slides in memory.
    pub fn set_hydration_policy(
        &self,
        context_id: ContextId,
        marker: BlockId,
        window_size: u32,
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_hydration (context_id, marker, window_size)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id) DO UPDATE SET marker = ?2, window_size = ?3",
            params![
                blob_param(context_id.as_bytes()),
                marker.to_key(),
                window_size,
            ],
        )?;
        Ok(())
    }

    /// Read the hydration window policy for `context_id`, or `None` when unset —
    /// `None` means hydrate everything (the default; every non-musician context).
    /// A *present but malformed* row (window < 1, or a marker that no longer
    /// parses) is corruption, and returns `Err(Validation)` so the caller fails
    /// the turn loudly. Silently degrading corrupt config to "hydrate everything"
    /// would disable the cost guard on a context driving at tempo (unbounded
    /// spend) — a silent fallback on a safety mechanism. Absent row = legible
    /// default (`Ok(None)`); malformed presence = loud failure.
    pub fn get_hydration_policy(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<(BlockId, u32)>> {
        let row = self
            .conn
            .query_row(
                "SELECT marker, window_size FROM context_hydration WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| {
                    let marker: String = row.get(0)?;
                    let window: i64 = row.get(1)?;
                    Ok((marker, window))
                },
            )
            .optional()?;
        let Some((marker_key, window)) = row else {
            return Ok(None);
        };
        // A 0/negative window or an unparseable marker is CORRUPT stored state,
        // not a legible policy. Refuse loudly (the caller fails the turn) rather
        // than silently degrade to full history — disabling the cost guard on a
        // context driving at tempo is a silent fallback on a safety mechanism.
        if window < 1 {
            return Err(KernelDbError::Validation(format!(
                "context {} hydration policy has window {window} (< 1) — corrupt",
                context_id.short()
            )));
        }
        match BlockId::from_key(&marker_key) {
            Some(marker) => Ok(Some((marker, window as u32))),
            None => Err(KernelDbError::Validation(format!(
                "context {} hydration marker {marker_key:?} is unparseable — corrupt",
                context_id.short()
            ))),
        }
    }

    /// Clear the hydration policy for `context_id` → revert to hydrating
    /// everything. Returns the row count (0 or 1).
    pub fn clear_hydration_policy(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM context_hydration WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    // ========================================================================
    // Tracks (clock domains — docs/tracks.md Stage 1)
    // ========================================================================

    /// Upsert the durable `PersistedTrack` row. Called by the scheduler on every
    /// policy mutation (attach, tempo, playhead advance) so the row always
    /// reflects the running state and a cold-start can reconstruct it.
    /// In-place (PK is `track_id`): a tempo change or playhead advance updates
    /// one row without inserting a duplicate.
    pub fn upsert_track(&self, t: &PersistedTrack) -> KernelDbResult<()> {
        Self::write_track(&self.conn, t)
    }

    /// Upsert a track row against `conn` (a `&Connection` or `&Transaction` via
    /// deref). Shared by `upsert_track` and transactional callers. Does NOT commit.
    fn write_track(conn: &Connection, t: &PersistedTrack) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO tracks
                 (track_id, period_ms, beats_per_phrase, playhead_tick, playing)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(track_id) DO UPDATE SET
                 period_ms = ?2, beats_per_phrase = ?3,
                 playhead_tick = ?4, playing = ?5",
            params![
                t.track_id,
                t.period_ms as i64,
                t.beats_per_phrase as i64,
                t.playhead_tick,
                t.playing as i64,
            ],
        )?;
        Ok(())
    }

    /// Read the persisted track for `track_id`, or `None` when no such track exists.
    /// A *present but malformed* row — an empty `track_id` or a zero `period_ms`
    /// (a beat that never advances) — is corruption and returns `Err(Validation)`
    /// so the caller refuses to reconstruct a broken clock.
    pub fn get_track(&self, track_id: &str) -> KernelDbResult<Option<PersistedTrack>> {
        if track_id.is_empty() {
            return Err(KernelDbError::Validation(
                "get_track called with an empty track_id — corrupt (no identity)".into(),
            ));
        }
        // Read raw signed integers and validate BEFORE casting. SQLite INTEGER is
        // signed; the upsert only ever writes `u64 as i64` (≥ 0). A negative is
        // corruption; silently wrapping to u64 would produce nonsense cadences.
        let row = self
            .conn
            .query_row(
                "SELECT period_ms, beats_per_phrase, playhead_tick, playing
                 FROM tracks WHERE track_id = ?1",
                params![track_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((period_ms, beats_per_phrase, playhead_tick, playing)) = row else {
            return Ok(None);
        };
        if period_ms < 0 || beats_per_phrase < 0 {
            return Err(KernelDbError::Validation(format!(
                "track {track_id} has a negative field (period_ms={period_ms}, \
                 beats_per_phrase={beats_per_phrase}) — corrupt"
            )));
        }
        if period_ms == 0 {
            return Err(KernelDbError::Validation(format!(
                "track {track_id} has a zero period_ms — corrupt (a beat that never advances)"
            )));
        }
        Ok(Some(PersistedTrack {
            track_id: track_id.to_string(),
            period_ms: period_ms as u64,
            beats_per_phrase: beats_per_phrase as u64,
            playhead_tick,
            playing: playing != 0,
        }))
    }

    /// List all persisted tracks. Order is not guaranteed.
    pub fn list_tracks(&self) -> KernelDbResult<Vec<PersistedTrack>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, period_ms, beats_per_phrase, playhead_tick, playing
             FROM tracks",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut tracks = Vec::new();
        for row in rows {
            let (track_id, period_ms, beats_per_phrase, playhead_tick, playing) = row?;
            tracks.push(PersistedTrack {
                track_id,
                period_ms: period_ms as u64,
                beats_per_phrase: beats_per_phrase as u64,
                playhead_tick,
                playing: playing != 0,
            });
        }
        Ok(tracks)
    }

    /// Delete a track row by `track_id`. Cascades to all its `attachments` rows
    /// via the FK `ON DELETE CASCADE`. Returns the row count (0 or 1).
    pub fn delete_track(&self, track_id: &str) -> KernelDbResult<u64> {
        let deleted = self
            .conn
            .execute("DELETE FROM tracks WHERE track_id = ?1", params![track_id])?;
        Ok(deleted as u64)
    }

    // ========================================================================
    // Attachments (context → track binding — docs/tracks.md §2+3)
    // ========================================================================

    /// Upsert an attachment row. In-place (PK is `(track_id, context_id)`):
    /// a wakeup change or ooda-arm toggle updates one row.
    pub fn upsert_attachment(&self, a: &PersistedAttachment) -> KernelDbResult<()> {
        Self::write_attachment(&self.conn, a)
    }

    /// Upsert an attachment against `conn` (a `&Connection` or `&Transaction` via
    /// deref). Shared by `upsert_attachment` and transactional callers (the fork
    /// copy). Does NOT commit.
    fn write_attachment(conn: &Connection, a: &PersistedAttachment) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO attachments
                 (track_id, context_id, wakeup_every, rotate_every_phrases, ooda_armed)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(track_id, context_id) DO UPDATE SET
                 wakeup_every = ?3, rotate_every_phrases = ?4, ooda_armed = ?5",
            params![
                a.track_id,
                blob_param(a.context_id.as_bytes()),
                a.wakeup_every as i64,
                a.rotate_every_phrases.map(|n| n as i64),
                a.ooda_armed as i64,
            ],
        )?;
        Ok(())
    }

    /// Read one attachment by `(track_id, context_id)`, or `None` when absent.
    pub fn get_attachment(
        &self,
        track_id: &str,
        context_id: ContextId,
    ) -> KernelDbResult<Option<PersistedAttachment>> {
        let row = self
            .conn
            .query_row(
                "SELECT wakeup_every, rotate_every_phrases, ooda_armed
                 FROM attachments WHERE track_id = ?1 AND context_id = ?2",
                params![track_id, blob_param(context_id.as_bytes())],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((wakeup_every, rotate_every_phrases, ooda_armed)) = row else {
            return Ok(None);
        };
        Ok(Some(PersistedAttachment {
            track_id: track_id.to_string(),
            context_id,
            wakeup_every: wakeup_every as u64,
            rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
            ooda_armed: ooda_armed != 0,
        }))
    }

    /// List all attachments for a track (all contexts attached to `track_id`).
    pub fn list_attachments_for_track(
        &self,
        track_id: &str,
    ) -> KernelDbResult<Vec<PersistedAttachment>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments WHERE track_id = ?1",
        )?;
        let rows = stmt.query_map(params![track_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (ctx_bytes, wakeup_every, rotate_every_phrases, ooda_armed) = row?;
            let context_id = ContextId::try_from_slice(&ctx_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    "invalid ContextId bytes in attachments row".into(),
                )
            })?;
            out.push(PersistedAttachment {
                track_id: track_id.to_string(),
                context_id,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            });
        }
        Ok(out)
    }

    /// List all attachments for a context (all tracks this context is attached to).
    pub fn list_attachments_for_context(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Vec<PersistedAttachment>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments WHERE context_id = ?1",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (track_id, wakeup_every, rotate_every_phrases, ooda_armed) = row?;
            out.push(PersistedAttachment {
                track_id,
                context_id,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            });
        }
        Ok(out)
    }

    /// Delete one attachment by `(track_id, context_id)`. Returns the row count
    /// (0 = already absent, 1 = deleted).
    pub fn delete_attachment(&self, track_id: &str, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM attachments WHERE track_id = ?1 AND context_id = ?2",
            params![track_id, blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Copy all attachments from `source` context to `child` context, keyed on
    /// the SAME `track_id`. Called inside the `insert_forked_context` transaction
    /// so the attachment inheritance is atomic with the context-row write.
    ///
    /// This is the attachment equivalent of the old `write_beat_state` fork copy
    /// (docs/tracks.md §3 — "the child inherits the bind at fork"). A non-musician
    /// source has no attachments, so the copy is a clean no-op in that case.
    /// Does NOT commit.
    fn copy_attachments_for_fork(
        conn: &Connection,
        source: ContextId,
        child: ContextId,
    ) -> KernelDbResult<()> {
        // Read all source attachments first so the transaction holds only writes.
        let mut stmt = conn.prepare(
            "SELECT track_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments WHERE context_id = ?1",
        )?;
        let rows: Vec<_> = stmt
            .query_map(params![blob_param(source.as_bytes())], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        for (track_id, wakeup_every, rotate_every_phrases, ooda_armed) in rows {
            let a = PersistedAttachment {
                track_id,
                context_id: child,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            };
            Self::write_attachment(conn, &a)?;
        }
        Ok(())
    }

    // ========================================================================
    // Context Config Fork + Workspace Query
    // ========================================================================

    /// Copy shell config + env vars + capability binding from source context
    /// to target. Called during all fork operations. The binding copy makes
    /// permissions follow the fork — under deny-by-default a fork would
    /// otherwise start with no loadout and be locked out.
    ///
    /// Atomic: the three copies land in ONE transaction. A fork that fails
    /// partway must leave NO partial config behind — a half-copied loadout
    /// would lock the fork out (or grant a stale subset), and the caller has
    /// already committed the context row, so a silent partial here would
    /// strand a misconfigured context. The source is read up front so the
    /// transaction holds only the writes.
    pub fn fork_context_config(&mut self, source: ContextId, target: ContextId) -> KernelDbResult<()> {
        let shell = self.get_context_shell(source)?;
        let env = self.get_context_env(source)?;
        let binding = self.get_context_binding(source)?;

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
        if let Some(binding) = binding {
            Self::write_binding(&tx, target, &binding)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Copy the capability binding (flags + instances + tools + facades +
    /// sticky names) from `source` to `target`. No-op if the source has no
    /// binding. The child can later attenuate (self-narrow) but inherits the
    /// parent's loadout as its starting point.
    pub fn copy_context_binding(
        &mut self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        match self.get_context_binding(source)? {
            Some(binding) => {
                self.upsert_context_binding(target, &binding)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Get workspace paths for a context (via contexts.workspace_id FK).
    /// Returns None if context has no workspace bound.
    pub fn context_workspace_paths(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<Vec<WorkspacePathRow>>> {
        let ctx = self
            .get_context(context_id)?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", context_id.short())))?;
        match ctx.workspace_id {
            Some(ws_id) => {
                let paths = self.list_workspace_paths(ws_id)?;
                Ok(Some(paths))
            }
            None => Ok(None),
        }
    }

    // ========================================================================
    // Workspace Permission Checking
    // ========================================================================

    /// Check whether a path is allowed by a context's workspace.
    ///
    /// Returns `None` if the context has no workspace (unbound = kernel perimeter
    /// defaults, no restriction). Returns `Some(read_only)` if the path falls
    /// under a workspace path. Returns an error if the path is outside all
    /// workspace paths (bound context, path not in scope).
    pub fn check_workspace_path(
        &self,
        context_id: ContextId,
        path: &str,
    ) -> KernelDbResult<Option<bool>> {
        let ws_paths = match self.context_workspace_paths(context_id)? {
            None => return Ok(None), // unbound context — no workspace restriction
            Some(paths) => paths,
        };

        // Find longest-prefix match among workspace paths
        let mut best: Option<&WorkspacePathRow> = None;
        for wp in &ws_paths {
            if path == wp.path || path.starts_with(&format!("{}/", wp.path)) {
                match best {
                    None => best = Some(wp),
                    Some(prev) if wp.path.len() > prev.path.len() => best = Some(wp),
                    _ => {}
                }
            }
        }

        match best {
            Some(wp) => Ok(Some(wp.read_only)),
            None => Err(KernelDbError::Validation(format!(
                "path '{}' is outside workspace scope",
                path,
            ))),
        }
    }

    // ========================================================================
    // Phase 4A: Additional methods for kj commands
    // ========================================================================

    /// Delete a structural edge between source and target.
    ///
    /// Used by `kj context move` to reparent a context.
    pub fn delete_structural_edge(
        &self,
        source: ContextId,
        target: ContextId,
    ) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_edges
             WHERE source_id = ?1 AND target_id = ?2 AND kind = 'structural'",
            params![blob_param(source.as_bytes()), blob_param(target.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    /// Delete a single drift edge by its UUID.
    ///
    /// Companion to `kj drift edge rm` — `kj drift history` emits these
    /// UUIDs as iteration handles. Kind filter pins this to drift edges
    /// only so a stray `delete_drift_edge` can't accidentally remove a
    /// structural edge (which would orphan a context's parent link).
    pub fn delete_drift_edge(&self, edge_id: uuid::Uuid) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_edges
             WHERE edge_id = ?1 AND kind = 'drift'",
            params![edge_id.as_bytes().as_slice()],
        )?;
        Ok(deleted > 0)
    }

    /// Hard-delete a context and all its edges (CASCADE).
    ///
    /// Used by `kj context remove`. This is permanent — use `archive_context`
    /// for soft delete.
    pub fn delete_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM contexts WHERE context_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    /// Count contexts using a specific preset.
    pub fn contexts_using_preset(&self, preset_id: PresetId) -> KernelDbResult<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts
             WHERE preset_id = ?1 AND archived_at IS NULL",
            params![blob_param(preset_id.as_bytes())],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count contexts using a specific workspace.
    pub fn contexts_using_workspace(&self, workspace_id: WorkspaceId) -> KernelDbResult<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts
             WHERE workspace_id = ?1 AND archived_at IS NULL",
            params![blob_param(workspace_id.as_bytes())],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Find the context that currently holds a given label.
    pub fn find_context_by_label(&self, label: &str) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, label, provider, model,
                    system_prompt, consent_mode, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at
             FROM contexts WHERE label = ?1",
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }
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
    let consent_str: String = row.get(5)?;
    let state_str: String = row.get(6)?;
    let fork_kind_str: Option<String> = row.get(11)?;

    Ok(ContextRow {
        context_id: read_context_id(row, 0)?,
        label: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        system_prompt: row.get(4)?,
        consent_mode: consent_mode_from_sql(&consent_str),
        context_state: context_state_from_sql(&state_str),
        context_type: row.get(7)?,
        created_at: row.get(8)?,
        created_by: read_principal_id(row, 9)?,
        forked_from: read_opt_context_id(row, 10)?,
        fork_kind: fork_kind_from_sql(fork_kind_str)?,
        archived_at: row.get(12)?,
        workspace_id: read_opt_workspace_id(row, 13)?,
        preset_id: read_opt_preset_id(row, 14)?,
        concluded_at: row.get(15)?,
    })
}

fn row_to_edge_row(row: &rusqlite::Row<'_>) -> SqliteResult<ContextEdgeRow> {
    let kind_str: String = row.get(3)?;
    Ok(ContextEdgeRow {
        edge_id: read_edge_id(row, 0)?,
        source_id: read_context_id(row, 1)?,
        target_id: read_context_id(row, 2)?,
        kind: edge_kind_from_sql(&kind_str),
        metadata: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn row_to_preset_row(row: &rusqlite::Row<'_>) -> SqliteResult<PresetRow> {
    let consent_str: String = row.get(6)?;

    Ok(PresetRow {
        preset_id: read_preset_id(row, 0)?,
        label: row.get(1)?,
        description: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        system_prompt: row.get(5)?,
        consent_mode: consent_mode_from_sql(&consent_str),
        created_at: row.get(7)?,
        created_by: read_principal_id(row, 8)?,
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
// Test helpers
// ============================================================================

#[cfg(test)]
fn make_context_row(label: Option<&str>) -> ContextRow {
    ContextRow {
        context_id: ContextId::new(),
        label: label.map(String::from),
        provider: None,
        model: None,
        system_prompt: None,
        consent_mode: ConsentMode::default(),
        context_state: ContextState::Live,
        context_type: "default".to_string(),
        created_at: now_millis() as i64,
        created_by: PrincipalId::new(),
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: None,
        preset_id: None,
        concluded_at: None,
    }
}

/// Insert both a document row and context row for a context.
/// Tests need this because contexts FK to documents.
#[cfg(test)]
fn insert_context_with_doc(db: &KernelDb, row: &ContextRow, ws_id: WorkspaceId) {
    db.insert_document(&DocumentRow {
        document_id: row.context_id,
        workspace_id: ws_id,
        doc_kind: DocKind::Conversation,
        language: None,
        path: None,
        created_at: row.created_at,
        created_by: row.created_by,
    })
    .unwrap();
    db.insert_context(row).unwrap();
}

/// Set up a test DB with the default workspace. Returns its WorkspaceId.
#[cfg(test)]
fn setup_test_db(db: &KernelDb) -> WorkspaceId {
    db.get_or_create_default_workspace(PrincipalId::system())
        .unwrap()
}

#[cfg(test)]
fn make_edge(source: ContextId, target: ContextId, kind: EdgeKind) -> ContextEdgeRow {
    ContextEdgeRow {
        edge_id: uuid::Uuid::now_v7(),
        source_id: source,
        target_id: target,
        kind,
        metadata: None,
        created_at: now_millis() as i64,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_kind_from_sql_fails_loud_on_unknown() {
        // NULL column → None; known value parses.
        assert!(fork_kind_from_sql(None).unwrap().is_none());
        assert_eq!(
            fork_kind_from_sql(Some("filtered".into())).unwrap(),
            Some(ForkKind::Filtered)
        );
        // A retired ('shallow') or corrupt value must ERROR, never silently
        // degrade to None — that would erase fork provenance.
        assert!(fork_kind_from_sql(Some("shallow".into())).is_err());
        assert!(fork_kind_from_sql(Some("garbage".into())).is_err());
    }

    fn insert_test_preset(db: &KernelDb, label: &str) -> PresetId {
        let pid = PresetId::new();
        db.insert_preset(&PresetRow {
            preset_id: pid,
            label: label.into(),
            description: None,
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
            created_at: now_millis() as i64,
            created_by: PrincipalId::new(),
        })
        .unwrap();
        pid
    }

    fn arg(name: &str, value: &str) -> PresetArg {
        PresetArg { arg_name: name.into(), arg_value: value.into() }
    }

    #[test]
    fn preset_args_roundtrip_verb_scoped_and_dedup() {
        let mut db = KernelDb::in_memory().unwrap();
        let pid = insert_test_preset(&db, "window");

        assert!(db.get_preset_args(pid, "fork").unwrap().is_empty());

        // Repeated --exclude plus a duplicate that must collapse via the PK.
        db.set_preset_args(
            pid,
            "fork",
            &[
                arg("exclude", "10:12"),
                arg("exclude", "0:5"),
                arg("exclude", "0:5"),
                arg("window", "16"),
            ],
        )
        .unwrap();

        // Ordered by (arg_name, arg_value); the dup is gone.
        assert_eq!(
            db.get_preset_args(pid, "fork").unwrap(),
            vec![arg("exclude", "0:5"), arg("exclude", "10:12"), arg("window", "16")]
        );

        // Verb-scoped: another verb is independent and doesn't disturb fork.
        db.set_preset_args(pid, "context", &[arg("model", "x")]).unwrap();
        assert_eq!(db.get_preset_args(pid, "fork").unwrap().len(), 3);

        // Replace semantics: re-setting fork wipes the prior set.
        db.set_preset_args(pid, "fork", &[arg("include", "end-5:")]).unwrap();
        assert_eq!(db.get_preset_args(pid, "fork").unwrap(), vec![arg("include", "end-5:")]);

        // Empty clears.
        db.set_preset_args(pid, "fork", &[]).unwrap();
        assert!(db.get_preset_args(pid, "fork").unwrap().is_empty());
    }

    #[test]
    fn preset_args_cascade_on_preset_delete() {
        let mut db = KernelDb::in_memory().unwrap();
        let pid = insert_test_preset(&db, "spawn");
        db.set_preset_args(pid, "fork", &[arg("include", ":0")]).unwrap();
        assert_eq!(db.get_preset_args(pid, "fork").unwrap().len(), 1);

        assert!(db.delete_preset(pid).unwrap());
        assert!(
            db.get_preset_args(pid, "fork").unwrap().is_empty(),
            "preset_args must cascade-delete with the preset"
        );
    }

    // ── 1. Schema idempotent ────────────────────────────────────────────

    #[test]
    fn schema_idempotent() {
        let db = KernelDb::in_memory().unwrap();
        // Apply schema again — should not error.
        db.conn.execute_batch(SCHEMA).unwrap();
    }

    // ── WAL checkpoint ──────────────────────────────────────────────────

    /// `checkpoint()` (TRUNCATE) must flush committed frames into the main
    /// file and shrink the `-wal` file back to zero. Guards the proactive
    /// checkpoint that compaction relies on so a bare-file read of the main
    /// `.db` stops lagging committed history (2026-06-11 forensics). On-disk
    /// db required — `in_memory()` has no WAL file.
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
            created_at: now_millis() as i64,
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

    // ── 2. Context lifecycle ────────────────────────────────────────────

    #[test]
    fn context_lifecycle() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("main"));
        let cid = row.context_id;

        insert_context_with_doc(&db, &row, ws_id);

        // Get
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.label, Some("main".into()));

        // Update label
        db.update_label(cid, Some("primary")).unwrap();
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.label, Some("primary".into()));

        // Archive
        assert!(db.archive_context(cid).unwrap());

        // list_active excludes
        let active = db.list_active_contexts().unwrap();
        assert!(active.is_empty());

        // list_all includes
        let all = db.list_all_contexts().unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].archived_at.is_some());
    }

    #[test]
    fn conclude_context_sets_state_timestamp_and_is_idempotent() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("worky"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        // Fresh context: open (no concluded_at, Live).
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.concluded_at, None);
        assert_eq!(loaded.context_state, ContextState::Live);

        // First conclude: newly concluded → true, stamps state + timestamp.
        assert!(db.conclude_context(cid).unwrap());
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.context_state, ContextState::Concluded);
        let stamp = loaded.concluded_at.expect("concluded_at set");
        assert!(stamp > 0);

        // Idempotent: re-conclude → false, original timestamp preserved.
        assert!(!db.conclude_context(cid).unwrap());
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.concluded_at, Some(stamp));

        // Concluded is NOT archived — still visible to list_active_contexts.
        let active = db.list_active_contexts().unwrap();
        assert_eq!(active.len(), 1, "concluded context stays active (not hidden)");
    }

    #[test]
    fn conclude_context_rejects_archived() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("gone"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert!(db.archive_context(cid).unwrap());
        // Archived contexts can't be concluded (guard in the UPDATE).
        assert!(!db.conclude_context(cid).unwrap());
    }

    // ── 3. Label validation: colon ──────────────────────────────────────

    #[test]
    fn label_validation_colon() {
        let db = KernelDb::in_memory().unwrap();
        let row = make_context_row(Some("my:label"));

        let err = db.insert_context(&row).unwrap_err();
        assert!(matches!(err, KernelDbError::InvalidLabel(_)));
    }

    // ── 4. Label uniqueness ────────────────────────────────────────────

    #[test]
    fn label_uniqueness() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let row1 = make_context_row(Some("shared"));
        insert_context_with_doc(&db, &row1, ws_id);

        // Same label → conflict (doc insert ok, context label conflicts).
        // Labels are globally unique within the kernel.
        let row2 = make_context_row(Some("shared"));
        db.insert_document(&DocumentRow {
            document_id: row2.context_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: row2.created_at,
            created_by: row2.created_by,
        })
        .unwrap();
        let err = db.insert_context(&row2).unwrap_err();
        assert!(matches!(err, KernelDbError::LabelConflict(_)));

        // NULL + NULL → OK (multiple unlabeled contexts allowed)
        let n1 = make_context_row(None);
        let n2 = make_context_row(None);
        insert_context_with_doc(&db, &n1, ws_id);
        insert_context_with_doc(&db, &n2, ws_id);
    }

    // ── 5. Fork lineage 3 deep ─────────────────────────────────────────

    #[test]
    fn fork_lineage_3_deep() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let root = make_context_row(Some("root"));
        insert_context_with_doc(&db, &root, ws_id);

        let mut child = make_context_row(Some("child"));
        child.forked_from = Some(root.context_id);
        child.fork_kind = Some(ForkKind::Full);
        insert_context_with_doc(&db, &child, ws_id);

        let mut grandchild = make_context_row(Some("grandchild"));
        grandchild.forked_from = Some(child.context_id);
        grandchild.fork_kind = Some(ForkKind::Filtered);
        insert_context_with_doc(&db, &grandchild, ws_id);

        let lineage = db.fork_lineage(grandchild.context_id).unwrap();
        assert_eq!(lineage.len(), 3);
        assert_eq!(lineage[0].0.context_id, grandchild.context_id);
        assert_eq!(lineage[0].1, 0);
        assert_eq!(lineage[1].0.context_id, child.context_id);
        assert_eq!(lineage[1].1, 1);
        assert_eq!(lineage[2].0.context_id, root.context_id);
        assert_eq!(lineage[2].1, 2);
    }

    // ── 6. Subtree snapshot ────────────────────────────────────────────

    #[test]
    fn subtree_snapshot() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let parent = make_context_row(Some("template"));
        insert_context_with_doc(&db, &parent, ws_id);

        let c1 = make_context_row(Some("child1"));
        insert_context_with_doc(&db, &c1, ws_id);
        db.insert_edge(&make_edge(
            parent.context_id,
            c1.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();

        let c2 = make_context_row(Some("child2"));
        insert_context_with_doc(&db, &c2, ws_id);
        db.insert_edge(&make_edge(
            parent.context_id,
            c2.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();

        let snapshot = db.subtree_snapshot(parent.context_id).unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].1, 0); // parent at depth 0
        assert_eq!(snapshot[1].1, 1); // child at depth 1
        assert_eq!(snapshot[2].1, 1); // child at depth 1
    }

    // ── 7. Structural edge unique ──────────────────────────────────────

    #[test]
    fn structural_edge_unique() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(Some("a"));
        let b = make_context_row(Some("b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        // First structural edge OK
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();

        // Duplicate structural → error
        let err = db
            .insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap_err();
        assert!(matches!(err, KernelDbError::LabelConflict(_)));

        // Same pair as drift → OK
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Drift))
            .unwrap();
    }

    // ── 8. Drift edge allows dupes ─────────────────────────────────────

    #[test]
    fn drift_edge_allows_dupes() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(None);
        let b = make_context_row(None);
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        // Multiple drift edges same pair → all stored
        for _ in 0..3 {
            db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Drift))
                .unwrap();
        }

        let edges = db.edges_from(a.context_id, Some(EdgeKind::Drift)).unwrap();
        assert_eq!(edges.len(), 3);
    }

    // ── 9. Cycle detection ─────────────────────────────────────────────

    #[test]
    fn cycle_detection() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(Some("cyc-a"));
        let b = make_context_row(Some("cyc-b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        // A → B structural OK
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();

        // B → A structural → CycleDetected
        let err = db
            .insert_edge(&make_edge(b.context_id, a.context_id, EdgeKind::Structural))
            .unwrap_err();
        assert!(matches!(err, KernelDbError::CycleDetected));

        // Self-loop also detected
        let c = make_context_row(Some("cyc-c"));
        insert_context_with_doc(&db, &c, ws_id);
        let err = db
            .insert_edge(&make_edge(c.context_id, c.context_id, EdgeKind::Structural))
            .unwrap_err();
        assert!(matches!(err, KernelDbError::CycleDetected));
    }

    // ── 10. Preset lifecycle ───────────────────────────────────────────

    #[test]
    fn preset_lifecycle() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let mut preset = PresetRow {
            preset_id: PresetId::new(),
                        label: "opus-research".into(),
            description: Some("Deep research preset".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-6".into()),
            system_prompt: None,
            consent_mode: ConsentMode::Autonomous,
            created_at: now,
            created_by: creator,
        };

        db.insert_preset(&preset).unwrap();

        // Get by label
        let loaded = db
            .get_preset_by_label("opus-research")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.model, Some("claude-opus-4-6".into()));

        // Update
        preset.model = Some("claude-sonnet-4-6".into());
        db.update_preset(&preset).unwrap();
        let loaded = db.get_preset(preset.preset_id).unwrap().unwrap();
        assert_eq!(loaded.model, Some("claude-sonnet-4-6".into()));

        // Delete
        assert!(db.delete_preset(preset.preset_id).unwrap());
        assert!(db.get_preset(preset.preset_id).unwrap().is_none());
    }

    // ── 11. Workspace lifecycle ────────────────────────────────────────

    #[test]
    fn workspace_lifecycle() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "kaijutsu".into(),
            description: Some("Main project".into()),
            created_at: now,
            created_by: creator,
            archived_at: None,
        };

        db.insert_workspace(&ws).unwrap();

        // Add paths
        let p1 = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu".into(),
            read_only: false,
            created_at: now,
        };
        let p2 = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaish".into(),
            read_only: false,
            created_at: now,
        };
        db.insert_workspace_path(&p1).unwrap();
        db.insert_workspace_path(&p2).unwrap();

        // List paths
        let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
        assert_eq!(paths.len(), 2);

        // Delete one path
        assert!(
            db.delete_workspace_path(ws.workspace_id, "/home/user/src/kaish")
                .unwrap()
        );
        let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
        assert_eq!(paths.len(), 1);

        // Archive workspace
        assert!(db.archive_workspace(ws.workspace_id).unwrap());
        let active = db.list_workspaces().unwrap();
        assert!(active.is_empty());
    }

    // ── 12. Workspace soft delete ──────────────────────────────────────

    #[test]
    fn workspace_soft_delete() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "project".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();

        // Create context referencing this workspace
        let mut ctx = make_context_row(Some("ctx-ws"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Archive workspace
        db.archive_workspace(ws.workspace_id).unwrap();

        // Context still references the workspace
        let loaded = db.get_context(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.workspace_id, Some(ws.workspace_id));

        // get_workspace still returns it (it's archived, not deleted)
        let loaded_ws = db.get_workspace(ws.workspace_id).unwrap().unwrap();
        assert!(loaded_ws.archived_at.is_some());
    }

    // ── 13. Drift push creates edge ────────────────────────────────────

    #[test]
    fn drift_push_creates_edge() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(Some("source"));
        let b = make_context_row(Some("target"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        // Simulate drift push → create drift edge
        let edge1 = make_edge(a.context_id, b.context_id, EdgeKind::Drift);
        db.insert_edge(&edge1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1));

        let edge2 = make_edge(a.context_id, b.context_id, EdgeKind::Drift);
        db.insert_edge(&edge2).unwrap();

        // edges_from returns both
        let edges = db.edges_from(a.context_id, Some(EdgeKind::Drift)).unwrap();
        assert_eq!(edges.len(), 2);

        // drift_provenance returns newest first
        let prov = db.drift_provenance(a.context_id).unwrap();
        assert_eq!(prov.len(), 2);
        assert!(prov[0].created_at >= prov[1].created_at);
    }

    // ── 14. Context DAG recursive ──────────────────────────────────────

    #[test]
    fn context_dag_recursive() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        // Create a 5-node tree: root → [a, b], a → [c, d]
        let root = make_context_row(Some("root"));
        let a = make_context_row(Some("a"));
        let b = make_context_row(Some("b"));
        let c = make_context_row(Some("c"));
        let d = make_context_row(Some("d"));

        for ctx in [&root, &a, &b, &c, &d] {
            insert_context_with_doc(&db, ctx, ws_id);
        }

        db.insert_edge(&make_edge(
            root.context_id,
            a.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();
        db.insert_edge(&make_edge(
            root.context_id,
            b.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();
        db.insert_edge(&make_edge(a.context_id, c.context_id, EdgeKind::Structural))
            .unwrap();
        db.insert_edge(&make_edge(a.context_id, d.context_id, EdgeKind::Structural))
            .unwrap();

        let dag = db.context_dag().unwrap();
        assert_eq!(dag.len(), 5);

        // Root at depth 0
        assert_eq!(dag[0].0.context_id, root.context_id);
        assert_eq!(dag[0].1, 0);

        // a, b at depth 1
        let depth1: Vec<_> = dag.iter().filter(|(_, d)| *d == 1).collect();
        assert_eq!(depth1.len(), 2);

        // c, d at depth 2
        let depth2: Vec<_> = dag.iter().filter(|(_, d)| *d == 2).collect();
        assert_eq!(depth2.len(), 2);
    }

    // ── 15. Tag resolution (label + prefix) ────────────────────────────

    #[test]
    fn tag_resolution() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let ctx1 = make_context_row(Some("opusplan"));
        let ctx2 = make_context_row(Some("sonnet"));
        insert_context_with_doc(&db, &ctx1, ws_id);
        insert_context_with_doc(&db, &ctx2, ws_id);

        // Exact label match
        let resolved = db.resolve_context("opusplan").unwrap();
        assert_eq!(resolved, ctx1.context_id);

        // Prefix match
        let resolved = db.resolve_context("opus").unwrap();
        assert_eq!(resolved, ctx1.context_id);
    }

    // ── 16. Resolve context basic ──────────────────────────────────────

    #[test]
    fn resolve_context_basic() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let ctx = make_context_row(Some("unique-label"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Exact label
        let r = db.resolve_context("unique-label").unwrap();
        assert_eq!(r, ctx.context_id);

        // Label prefix
        let r = db.resolve_context("unique").unwrap();
        assert_eq!(r, ctx.context_id);

        // Hex prefix
        let hex = ctx.context_id.to_hex();
        let r = db.resolve_context(&hex[..8]).unwrap();
        assert_eq!(r, ctx.context_id);

        // Not found
        let err = db.resolve_context("nonexistent").unwrap_err();
        assert!(matches!(err, KernelDbError::NotFound(_)));
    }

    // ── 17. Null labels coexist ────────────────────────────────────────

    #[test]
    fn null_labels_coexist() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        // Insert 5 contexts with NULL label — all succeed
        for _ in 0..5 {
            let row = make_context_row(None);
            insert_context_with_doc(&db, &row, ws_id);
        }

        let all = db.list_all_contexts().unwrap();
        assert_eq!(all.len(), 5);
    }

    // ── 18. Archive excludes from active + structural_children ─────────

    #[test]
    fn archive_excludes_from_active() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let parent = make_context_row(Some("parent"));
        let child = make_context_row(Some("child"));
        insert_context_with_doc(&db, &parent, ws_id);
        insert_context_with_doc(&db, &child, ws_id);
        db.insert_edge(&make_edge(
            parent.context_id,
            child.context_id,
            EdgeKind::Structural,
        ))
        .unwrap();

        // Before archive
        let children = db.structural_children(parent.context_id).unwrap();
        assert_eq!(children.len(), 1);

        // Archive child
        db.archive_context(child.context_id).unwrap();

        // list_active excludes
        let active = db.list_active_contexts().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].context_id, parent.context_id);

        // structural_children excludes archived
        let children = db.structural_children(parent.context_id).unwrap();
        assert!(children.is_empty());
    }

    // ── 19. Context edge cascade ───────────────────────────────────────

    #[test]
    fn context_edge_cascade() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(Some("edge-a"));
        let b = make_context_row(Some("edge-b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Structural))
            .unwrap();
        db.insert_edge(&make_edge(a.context_id, b.context_id, EdgeKind::Drift))
            .unwrap();

        // Both edges exist
        let all = db.edges_from(a.context_id, None).unwrap();
        assert_eq!(all.len(), 2);

        // Delete context a → edges cascade
        db.conn
            .execute(
                "DELETE FROM contexts WHERE context_id = ?1",
                params![blob_param(a.context_id.as_bytes())],
            )
            .unwrap();

        // Edges gone
        let all = db.edges_from(a.context_id, None).unwrap();
        assert!(all.is_empty());
    }

    // ── 20. Context row to_context conversion ──────────────────────────

    #[test]
    fn context_row_to_context() {
        let creator = PrincipalId::new();
        let parent = ContextId::new();
        let now = now_millis() as i64;

        let row = ContextRow {
            context_id: ContextId::new(),
                        label: Some("test".into()),
            provider: Some("anthropic".into()),
            model: Some("opus".into()),
            system_prompt: None,
            consent_mode: ConsentMode::Autonomous,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: now,
            created_by: creator,
            forked_from: Some(parent),
            fork_kind: Some(ForkKind::Full),
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
        };

        let ctx = row.to_context();
        assert_eq!(ctx.id, row.context_id);
        assert_eq!(ctx.label, Some("test".into()));
        assert_eq!(ctx.forked_from, Some(parent));
        assert_eq!(ctx.created_by, creator);
        assert_eq!(ctx.created_at, now as u64);
    }

    // ── 21. Roundtrip: create context, read back, verify all 15 fields ──

    #[test]
    fn roundtrip_create_and_recover() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let creator = PrincipalId::new();
        let parent_id = ContextId::new();

        // Insert parent first (forked_from FK requires it to exist)
        let parent = ContextRow {
            context_id: parent_id,
                        label: Some("parent".into()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-6".into()),
            system_prompt: Some("You are helpful.".into()),
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: 1000,
            created_by: creator,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
        };
        insert_context_with_doc(&db, &parent, ws_id);

        // Insert child forked from parent
        let child_id = ContextId::new();
        let child = ContextRow {
            context_id: child_id,
                        label: Some("child-fork".into()),
            provider: Some("google".into()),
            model: Some("gemini-2.0-flash".into()),
            system_prompt: Some("Be concise.".into()),
            consent_mode: ConsentMode::Autonomous,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: 2000,
            created_by: creator,
            forked_from: Some(parent_id),
            fork_kind: Some(ForkKind::Full),
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
        };
        insert_context_with_doc(&db, &child, ws_id);

        // Read back and verify all 15 fields
        let recovered = db.get_context(child_id).unwrap().expect("child not found");
        assert_eq!(recovered.context_id, child_id);
        assert_eq!(recovered.label, Some("child-fork".into()));
        assert_eq!(recovered.provider, Some("google".into()));
        assert_eq!(recovered.model, Some("gemini-2.0-flash".into()));
        assert_eq!(recovered.system_prompt, Some("Be concise.".into()));
        assert_eq!(recovered.consent_mode, ConsentMode::Autonomous);
        assert_eq!(recovered.created_at, 2000);
        assert_eq!(recovered.created_by, creator);
        assert_eq!(recovered.forked_from, Some(parent_id));
        assert_eq!(recovered.fork_kind, Some(ForkKind::Full));
        assert!(recovered.archived_at.is_none());
        assert!(recovered.workspace_id.is_none());
        assert!(recovered.preset_id.is_none());

        // Verify list_active_contexts returns both
        let active = db.list_active_contexts().unwrap();
        assert_eq!(active.len(), 2);

        // Verify to_context() preserves forked_from
        let ctx = recovered.to_context();
        assert_eq!(ctx.forked_from, Some(parent_id));
        assert_eq!(ctx.created_by, creator);

        // Verify update_model roundtrip
        db.update_model(child_id, Some("deepseek"), Some("deepseek-r1"))
            .unwrap();
        let updated = db.get_context(child_id).unwrap().unwrap();
        assert_eq!(updated.provider, Some("deepseek".into()));
        assert_eq!(updated.model, Some("deepseek-r1".into()));
    }

    // ── 22. FK violation produces Validation, not LabelConflict ──────────

    #[test]
    fn fk_violation_is_validation_error() {
        let db = KernelDb::in_memory().unwrap();
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
            created_at: now_millis() as i64,
            created_by: PrincipalId::new(),
        })
        .unwrap();

        let row = ContextRow {
            context_id: ctx_id,
                        label: Some("fk-test".into()),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::default(),
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: now_millis() as i64,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: Some(WorkspaceId::new()), // doesn't exist
            preset_id: None,
            concluded_at: None,
        };
        let err = db.insert_context(&row).unwrap_err();
        assert!(
            matches!(err, KernelDbError::Validation(_)),
            "expected Validation for FK violation, got: {err}"
        );
    }

    // ── 23. Context shell CRUD ────────────────────────────────────────

    #[test]
    fn context_shell_upsert_and_get() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("shell-test"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Initially none
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());

        // Insert
        let row = ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/home/user/src/kaijutsu".into()),
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row).unwrap();

        let loaded = db.get_context_shell(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.cwd, Some("/home/user/src/kaijutsu".into()));

        // Update (upsert changes cwd)
        let row2 = ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/tmp/work".into()),
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row2).unwrap();

        let loaded = db.get_context_shell(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.cwd, Some("/tmp/work".into()));
    }

    #[test]
    fn context_shell_get_unknown() {
        let db = KernelDb::in_memory().unwrap();
        assert!(db.get_context_shell(ContextId::new()).unwrap().is_none());
    }

    #[test]
    fn context_shell_copy() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("src"));
        let tgt = make_context_row(Some("tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Copy from context with shell config
        let row = ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/project".into()),
            updated_at: now_millis() as i64,
        };
        db.upsert_context_shell(&row).unwrap();

        assert!(
            db.copy_context_shell(src.context_id, tgt.context_id)
                .unwrap()
        );

        let copied = db.get_context_shell(tgt.context_id).unwrap().unwrap();
        assert_eq!(copied.cwd, Some("/home/user/project".into()));
    }

    #[test]
    fn context_shell_copy_empty() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("src"));
        let tgt = make_context_row(Some("tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Copy from context with no shell config → returns false
        assert!(
            !db.copy_context_shell(src.context_id, tgt.context_id)
                .unwrap()
        );
        assert!(db.get_context_shell(tgt.context_id).unwrap().is_none());
    }

    #[test]
    fn context_shell_cascade_delete() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.upsert_context_shell(&ContextShellRow {
            context_id: ctx.context_id,
            cwd: Some("/tmp".into()),
            updated_at: now_millis() as i64,
        })
        .unwrap();
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_some());

        // Delete context → shell row should cascade
        db.delete_context(ctx.context_id).unwrap();
        assert!(db.get_context_shell(ctx.context_id).unwrap().is_none());
    }

    // ── 23b. Context tool bindings CRUD (Phase 5, D-54) ──────────────
    //
    // Normalized schema: parent `context_bindings` + `_instances` (ordered)
    // + `_names` (sticky map). The get path reconstructs `ContextToolBinding`
    // by joining; the upsert path writes all three tables transactionally.

    fn binding_with(
        instances: &[&str],
        names: &[(&str, &str, &str)], // (visible, instance, original)
    ) -> ContextToolBinding {
        let mut b = ContextToolBinding::new();
        b.allowed_instances = instances.iter().map(|s| InstanceId::new(*s)).collect();
        for (visible, inst, tool) in names {
            b.name_map
                .insert((*visible).into(), (InstanceId::new(*inst), (*tool).into()));
        }
        b
    }

    #[test]
    fn context_binding_roundtrip_preserves_order_and_sticky_names() {
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("binding-roundtrip"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let original = binding_with(
            &["builtin.block", "builtin.file", "external.gpal"],
            &[
                ("read", "builtin.file", "file_read"),
                ("write", "builtin.file", "file_write"),
                ("consult", "external.gpal", "consult_gemini"),
            ],
        );
        db.upsert_context_binding(ctx.context_id, &original).unwrap();

        let loaded = db
            .get_context_binding(ctx.context_id)
            .unwrap()
            .expect("binding should exist after upsert");

        // Order of `allowed_instances` must survive the roundtrip (D-20:
        // order is the tiebreaker for Auto resolution).
        assert_eq!(loaded.allowed_instances.len(), 3);
        assert_eq!(loaded.allowed_instances[0].as_str(), "builtin.block");
        assert_eq!(loaded.allowed_instances[1].as_str(), "builtin.file");
        assert_eq!(loaded.allowed_instances[2].as_str(), "external.gpal");

        // Name map content must match exactly — sticky resolution is the
        // whole point of persisting name_map across restart.
        assert_eq!(loaded.name_map.len(), 3);
        assert_eq!(
            loaded.name_map.get("read").unwrap(),
            &(InstanceId::new("builtin.file"), "file_read".into())
        );
        assert_eq!(
            loaded.name_map.get("consult").unwrap(),
            &(InstanceId::new("external.gpal"), "consult_gemini".into())
        );
    }

    #[test]
    fn context_binding_roundtrip_preserves_tool_and_facade_grants() {
        use crate::mcp::Capability;
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("binding-caps"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // A tool-granular role bundle: no instance-wide grants, just specific
        // tools plus a facade. This is the toolie shape (slice 5).
        let mut original = ContextToolBinding::new();
        original.grant(Capability::Tool {
            instance: InstanceId::new("builtin.file"),
            tool: "read".into(),
        });
        original.grant(Capability::Tool {
            instance: InstanceId::new("builtin.file"),
            tool: "grep".into(),
        });
        original.grant(Capability::Facade("shell".into()));
        db.upsert_context_binding(ctx.context_id, &original).unwrap();

        let loaded = db
            .get_context_binding(ctx.context_id)
            .unwrap()
            .expect("binding should exist after upsert");

        assert!(
            loaded.allowed_instances.is_empty(),
            "no instance-wide grants were written"
        );
        assert!(!loaded.is_empty(), "tool/facade grants make it non-empty");
        assert_eq!(loaded.allowed_tools.len(), 2, "both tool grants survived");
        assert!(loaded.allows_tool(&InstanceId::new("builtin.file"), "read"));
        assert!(loaded.allows_tool(&InstanceId::new("builtin.file"), "grep"));
        assert!(
            !loaded.allows_tool(&InstanceId::new("builtin.file"), "write"),
            "ungranted sibling tool stays denied across restart"
        );
        assert_eq!(loaded.allowed_facades, vec!["shell".to_string()]);
    }

    #[test]
    fn context_binding_flags_roundtrip() {
        use crate::mcp::Capability;
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("binding-flags"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let mut original = ContextToolBinding::new();
        original.grant(Capability::AllInstances);
        original.grant(Capability::AllFacades);
        original.grant(Capability::Admin);
        original.grant(Capability::RcWrite);
        db.upsert_context_binding(ctx.context_id, &original).unwrap();

        let loaded = db
            .get_context_binding(ctx.context_id)
            .unwrap()
            .expect("binding should exist");
        assert!(loaded.all_instances, "all_instances survived restart");
        assert!(loaded.all_facades, "all_facades survived restart");
        assert!(loaded.binding_admin, "binding_admin survived restart");
        assert!(loaded.binding_rc_write, "binding_rc_write survived restart");
    }

    #[test]
    fn fork_copies_binding() {
        // Permissions follow the fork: copy_context_binding clones the parent's
        // loadout so a fork is not locked out under deny-by-default.
        use crate::mcp::Capability;
        let mut db = KernelDb::in_memory().unwrap();
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

    #[test]
    fn context_binding_get_absent_returns_none() {
        let db = KernelDb::in_memory().unwrap();
        // No context, no upsert: get must return None. Deny-by-default — the
        // broker treats None as "grants nothing", no first-touch fallback.
        assert!(db.get_context_binding(ContextId::new()).unwrap().is_none());
    }

    #[test]
    fn context_binding_upsert_replaces_wholesale() {
        // Phase 5 writes bindings as whole units; a second upsert must
        // wholly replace the children, not accumulate. Regression guard
        // against "leftover rows from a previous binding leak through."
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("binding-replace"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let first = binding_with(
            &["builtin.block", "builtin.file"],
            &[("read", "builtin.file", "file_read")],
        );
        db.upsert_context_binding(ctx.context_id, &first).unwrap();

        let second = binding_with(
            &["external.gpal"],
            &[("consult", "external.gpal", "consult_gemini")],
        );
        db.upsert_context_binding(ctx.context_id, &second).unwrap();

        let loaded = db.get_context_binding(ctx.context_id).unwrap().unwrap();
        assert_eq!(loaded.allowed_instances.len(), 1, "stale instances leaked");
        assert_eq!(loaded.allowed_instances[0].as_str(), "external.gpal");
        assert_eq!(loaded.name_map.len(), 1, "stale name_map entries leaked");
        assert!(loaded.name_map.contains_key("consult"));
        assert!(!loaded.name_map.contains_key("read"));
    }

    #[test]
    fn context_binding_delete_returns_whether_existed() {
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("binding-delete"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Delete of absent row returns false.
        assert!(!db.delete_context_binding(ctx.context_id).unwrap());

        db.upsert_context_binding(
            ctx.context_id,
            &binding_with(&["builtin.file"], &[]),
        )
        .unwrap();

        // Delete of present row returns true and clears children via CASCADE.
        assert!(db.delete_context_binding(ctx.context_id).unwrap());
        assert!(db.get_context_binding(ctx.context_id).unwrap().is_none());
    }

    #[test]
    fn context_binding_cascades_on_context_delete() {
        // Parent context gone → all three binding tables cascade-clear.
        // Guards against orphaned rows in binding_instances / binding_names.
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("binding-cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.upsert_context_binding(
            ctx.context_id,
            &binding_with(
                &["builtin.file", "builtin.block"],
                &[("read", "builtin.file", "file_read")],
            ),
        )
        .unwrap();
        assert!(db.get_context_binding(ctx.context_id).unwrap().is_some());

        db.delete_context(ctx.context_id).unwrap();
        assert!(db.get_context_binding(ctx.context_id).unwrap().is_none());

        // Children rows must also be gone (belt-and-suspenders on the
        // CASCADE — a future FK misstep would be caught here).
        let inst_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM context_binding_instances WHERE context_id = ?1",
                params![blob_param(ctx.context_id.as_bytes())],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inst_rows, 0);
        let name_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM context_binding_names WHERE context_id = ?1",
                params![blob_param(ctx.context_id.as_bytes())],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name_rows, 0);
    }

    // ── 23b. Hook persistence (hook persistence follow-up) ────────────
    //
    // Hooks are global (not per-context), so these tests don't need a
    // context row at all. Insert writes one row per entry with the DB
    // computing `insertion_idx` internally; load returns rows ordered
    // by `(phase, priority, insertion_idx)` so the broker can rebuild
    // `HookTable.entries` in the same Vec order as before restart.

    fn minimal_hook_row(id: &str, phase: &str, priority: i32) -> HookRow {
        HookRow {
            hook_id: id.into(),
            phase: phase.into(),
            priority,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "log".into(),
            action_builtin_name: None,
            action_kaish_body: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: Some("kaijutsu::hooks".into()),
            action_log_level: Some("info".into()),
        }
    }

    #[test]
    fn hook_insert_roundtrip_preserves_all_action_variants() {
        // One insert per action_kind. Round-trip must preserve every
        // variant-specific column and every match field so the broker's
        // reconstruction loses no information.
        let db = KernelDb::in_memory().unwrap();

        // Builtin-invoke with full match fields.
        let ctx_id = ContextId::new();
        let principal = PrincipalId::new();
        let builtin = HookRow {
            hook_id: "h-builtin".into(),
            phase: "pre_call".into(),
            priority: 10,
            match_instance: Some("builtin.*".into()),
            match_tool: Some("file_*".into()),
            match_context: Some(ctx_id),
            match_principal: Some(principal),
            action_kind: "builtin_invoke".into(),
            action_builtin_name: Some("tracing_audit".into()),
            action_kaish_body: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&builtin).unwrap();

        // ShortCircuit with is_error=true and no matches.
        let sc = HookRow {
            hook_id: "h-sc".into(),
            phase: "post_call".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "shortcircuit".into(),
            action_builtin_name: None,
            action_kaish_body: None,
            action_kaish_script_id: None,
            action_result_text: Some("synthetic".into()),
            action_is_error: Some(true),
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&sc).unwrap();

        // Deny.
        let deny = HookRow {
            hook_id: "h-deny".into(),
            phase: "pre_call".into(),
            priority: -5,
            match_instance: Some("builtin.file".into()),
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "deny".into(),
            action_builtin_name: None,
            action_kaish_body: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: Some("no writes".into()),
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&deny).unwrap();

        // Log.
        let log = minimal_hook_row("h-log", "on_notification", 0);
        db.insert_hook(&log).unwrap();

        // Kaish (reserved; rejected at admin time, but the row shape
        // must still round-trip so a manually-inserted row hydrates
        // predictably and the skip-with-warn path works).
        let kaish = HookRow {
            hook_id: "h-kaish".into(),
            phase: "list_tools".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: "kaish_invoke".into(),
            action_builtin_name: None,
            action_kaish_body: Some("script-42".into()),
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        db.insert_hook(&kaish).unwrap();

        let loaded = db.load_all_hooks().unwrap();
        assert_eq!(loaded.len(), 5);

        let by_id: std::collections::HashMap<String, HookRow> = loaded
            .into_iter()
            .map(|r| (r.hook_id.clone(), r))
            .collect();

        let b = by_id.get("h-builtin").unwrap();
        assert_eq!(b.phase, "pre_call");
        assert_eq!(b.priority, 10);
        assert_eq!(b.match_instance.as_deref(), Some("builtin.*"));
        assert_eq!(b.match_tool.as_deref(), Some("file_*"));
        assert_eq!(b.match_context, Some(ctx_id));
        assert_eq!(b.match_principal, Some(principal));
        assert_eq!(b.action_kind, "builtin_invoke");
        assert_eq!(b.action_builtin_name.as_deref(), Some("tracing_audit"));

        let s = by_id.get("h-sc").unwrap();
        assert_eq!(s.action_kind, "shortcircuit");
        assert_eq!(s.action_result_text.as_deref(), Some("synthetic"));
        assert_eq!(s.action_is_error, Some(true));

        let d = by_id.get("h-deny").unwrap();
        assert_eq!(d.action_kind, "deny");
        assert_eq!(d.action_deny_reason.as_deref(), Some("no writes"));
        assert_eq!(d.priority, -5);

        let l = by_id.get("h-log").unwrap();
        assert_eq!(l.action_kind, "log");
        assert_eq!(l.action_log_level.as_deref(), Some("info"));

        let k = by_id.get("h-kaish").unwrap();
        assert_eq!(k.action_kind, "kaish_invoke");
        assert_eq!(k.action_kaish_body.as_deref(), Some("script-42"));
    }

    #[test]
    fn hook_delete_returns_whether_existed() {
        let db = KernelDb::in_memory().unwrap();
        // Delete of absent id is false (idempotent cleanup).
        assert!(!db.delete_hook("nope").unwrap());

        db.insert_hook(&minimal_hook_row("keep", "pre_call", 0))
            .unwrap();
        assert!(db.delete_hook("keep").unwrap());
        // Second delete of same id is false.
        assert!(!db.delete_hook("keep").unwrap());
    }

    #[test]
    fn load_all_hooks_orders_by_phase_priority_then_insertion_idx() {
        // Tests the evaluation-law tiebreak (§4.3): priority ascending,
        // insertion-order tiebreak. Insert three hooks in pre_call with
        // identical priority; load must return them in insertion order.
        // Cross-phase, ordering within each phase must be preserved.
        let db = KernelDb::in_memory().unwrap();
        db.insert_hook(&minimal_hook_row("a", "pre_call", 5))
            .unwrap();
        db.insert_hook(&minimal_hook_row("b", "pre_call", 5))
            .unwrap();
        db.insert_hook(&minimal_hook_row("c", "pre_call", 1))
            .unwrap();
        db.insert_hook(&minimal_hook_row("x", "post_call", 0))
            .unwrap();
        db.insert_hook(&minimal_hook_row("y", "post_call", 0))
            .unwrap();

        let loaded = db.load_all_hooks().unwrap();
        let pre: Vec<&str> = loaded
            .iter()
            .filter(|r| r.phase == "pre_call")
            .map(|r| r.hook_id.as_str())
            .collect();
        // Priority 1 first, then priority 5 with insertion order (a, b).
        assert_eq!(pre, vec!["c", "a", "b"]);

        let post: Vec<&str> = loaded
            .iter()
            .filter(|r| r.phase == "post_call")
            .map(|r| r.hook_id.as_str())
            .collect();
        assert_eq!(post, vec!["x", "y"]);
    }

    #[test]
    fn hook_insert_same_id_errors() {
        // Primary-key collision is a load-bearing signal: the broker
        // should call `delete_hook` first for replace semantics rather
        // than relying on an implicit upsert. Lock that contract in.
        let db = KernelDb::in_memory().unwrap();
        db.insert_hook(&minimal_hook_row("dup", "pre_call", 0))
            .unwrap();
        let err = db
            .insert_hook(&minimal_hook_row("dup", "pre_call", 0))
            .unwrap_err();
        match err {
            KernelDbError::Db(_) => {}
            other => panic!("expected DB error on PK collision, got {other:?}"),
        }
    }

    // ── 24. Context env CRUD ──────────────────────────────────────────

    #[test]
    fn context_env_set_and_get() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("env-test"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Initially empty
        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert!(vars.is_empty());

        // Set vars
        db.set_context_env(ctx.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(ctx.context_id, "EDITOR", "vim").unwrap();

        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert_eq!(vars.len(), 2);
        // Ordered by key
        assert_eq!(vars[0].key, "EDITOR");
        assert_eq!(vars[0].value, "vim");
        assert_eq!(vars[1].key, "RUST_LOG");
        assert_eq!(vars[1].value, "debug");
    }

    #[test]
    fn context_env_upsert() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("env-upsert"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(ctx.context_id, "RUST_LOG", "trace")
            .unwrap();

        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].value, "trace");
    }

    #[test]
    fn context_env_get_empty() {
        let db = KernelDb::in_memory().unwrap();
        let vars = db.get_context_env(ContextId::new()).unwrap();
        assert!(vars.is_empty());
    }

    #[test]
    fn context_env_delete() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("env-del"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
        assert!(db.delete_context_env(ctx.context_id, "FOO").unwrap());
        assert!(!db.delete_context_env(ctx.context_id, "FOO").unwrap()); // already gone
        assert!(!db.delete_context_env(ctx.context_id, "NEVER_SET").unwrap());
    }

    #[test]
    fn context_env_clear() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("env-clear"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "A", "1").unwrap();
        db.set_context_env(ctx.context_id, "B", "2").unwrap();
        db.set_context_env(ctx.context_id, "C", "3").unwrap();

        assert_eq!(db.clear_context_env(ctx.context_id).unwrap(), 3);
        assert!(db.get_context_env(ctx.context_id).unwrap().is_empty());
    }

    #[test]
    fn context_env_copy() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("env-src"));
        let tgt = make_context_row(Some("env-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        db.set_context_env(src.context_id, "RUST_LOG", "debug")
            .unwrap();
        db.set_context_env(src.context_id, "EDITOR", "vim").unwrap();
        db.set_context_env(src.context_id, "SHELL", "/bin/bash")
            .unwrap();

        let count = db.copy_context_env(src.context_id, tgt.context_id).unwrap();
        assert_eq!(count, 3);

        let vars = db.get_context_env(tgt.context_id).unwrap();
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn context_env_cascade_delete() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("env-cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.set_context_env(ctx.context_id, "FOO", "bar").unwrap();
        db.set_context_env(ctx.context_id, "BAZ", "qux").unwrap();

        db.delete_context(ctx.context_id).unwrap();
        let vars = db.get_context_env(ctx.context_id).unwrap();
        assert!(vars.is_empty());
    }

    // ── Claude cache breakpoints ──────────────────────────────────────

    #[test]
    fn cache_breakpoints_empty_for_unknown_context() {
        let db = KernelDb::in_memory().unwrap();
        let bps = db.list_cache_breakpoints(ContextId::new()).unwrap();
        assert!(bps.is_empty());
    }

    #[test]
    fn cache_breakpoints_add_and_list_round_trip_all_variants() {
        // Round-trip every CacheTarget variant plus both CacheTtl values
        // — the table schema is right only if every shape survives a
        // write+read.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-rt"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Extended))
            .unwrap();
        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::System(CacheTtl::Ephemeral))
            .unwrap();
        db.add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(7, CacheTtl::Extended),
        )
        .unwrap();

        let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
        assert_eq!(bps.len(), 3);
        assert_eq!(bps[0], CacheTarget::Tools(CacheTtl::Extended));
        assert_eq!(bps[1], CacheTarget::System(CacheTtl::Ephemeral));
        assert_eq!(bps[2], CacheTarget::MessageIndex(7, CacheTtl::Extended));
    }

    #[test]
    fn hydration_policy_unset_is_none() {
        // No row → None → hydrate everything (the default for every context).
        let db = KernelDb::in_memory().unwrap();
        assert!(db.get_hydration_policy(ContextId::new()).unwrap().is_none());
    }

    #[test]
    fn hydration_policy_set_get_round_trip() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("hydra-rt"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let marker = BlockId::new(ctx.context_id, PrincipalId::new(), 4);
        db.set_hydration_policy(ctx.context_id, marker, 12).unwrap();

        let got = db.get_hydration_policy(ctx.context_id).unwrap();
        assert_eq!(got, Some((marker, 12)), "marker + window survive write→read");
    }

    #[test]
    fn hydration_policy_upsert_advances_marker_in_place() {
        // Advancing the marker (a durable revision) is a single in-place upsert,
        // not a second row — the PK is context_id.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("hydra-up"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let p1 = BlockId::new(ctx.context_id, PrincipalId::new(), 2);
        let p2 = BlockId::new(ctx.context_id, PrincipalId::new(), 9);
        db.set_hydration_policy(ctx.context_id, p1, 8).unwrap();
        db.set_hydration_policy(ctx.context_id, p2, 16).unwrap();

        assert_eq!(
            db.get_hydration_policy(ctx.context_id).unwrap(),
            Some((p2, 16)),
            "the second set overwrites the first (in-place advance)"
        );
    }

    #[test]
    fn hydration_policy_zero_window_is_loud_error() {
        // A window of 0 (corrupt row, or a hand-written DB edit) would window to
        // prefix-only and drop the current turn from the wire. Corrupt config is
        // a LOUD failure (the caller fails the turn), not a silent degrade to
        // hydrate-everything that disables the cost guard.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("hydra-zero"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let marker = BlockId::new(ctx.context_id, PrincipalId::new(), 3);
        db.set_hydration_policy(ctx.context_id, marker, 0).unwrap();
        assert!(
            matches!(
                db.get_hydration_policy(ctx.context_id),
                Err(KernelDbError::Validation(_))
            ),
            "a 0 window is corrupt → loud Validation error, not silent None"
        );
    }

    #[test]
    fn hydration_policy_unparseable_marker_is_loud_error() {
        // A stored marker that no longer parses is corruption in this one row.
        // Same stance as a bad window: refuse loudly, don't silently hydrate all.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("hydra-badmark"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Write a malformed marker straight into the row (bypass the typed setter).
        db.conn
            .execute(
                "INSERT INTO context_hydration (context_id, marker, window_size)
                 VALUES (?1, ?2, ?3)",
                params![blob_param(ctx.context_id.as_bytes()), "not-a-block-id", 8],
            )
            .unwrap();
        assert!(
            matches!(
                db.get_hydration_policy(ctx.context_id),
                Err(KernelDbError::Validation(_))
            ),
            "an unparseable marker is corrupt → loud Validation error"
        );
    }

    #[test]
    fn hydration_policy_clear_reverts_to_none() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("hydra-clear"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let marker = BlockId::new(ctx.context_id, PrincipalId::new(), 1);
        db.set_hydration_policy(ctx.context_id, marker, 4).unwrap();
        assert_eq!(db.clear_hydration_policy(ctx.context_id).unwrap(), 1);
        assert!(
            db.get_hydration_policy(ctx.context_id).unwrap().is_none(),
            "cleared policy reverts to hydrate-everything"
        );
    }

    // ── Tracks CRUD ───────────────────────────────────────────────────

    fn make_track(track_id: &str, period_ms: u64) -> PersistedTrack {
        PersistedTrack {
            track_id: track_id.to_string(),
            period_ms,
            beats_per_phrase: 16,
            playhead_tick: None,
            playing: false,
        }
    }

    fn make_attachment(track_id: &str, context_id: ContextId) -> PersistedAttachment {
        PersistedAttachment {
            track_id: track_id.to_string(),
            context_id,
            wakeup_every: 1,
            rotate_every_phrases: None,
            ooda_armed: false,
        }
    }

    #[test]
    fn track_upsert_get_round_trip() {
        let db = KernelDb::in_memory().unwrap();
        let t = make_track("bass", 250);
        db.upsert_track(&t).unwrap();
        assert_eq!(db.get_track("bass").unwrap(), Some(t), "track survives write→read");
    }

    #[test]
    fn track_absent_is_none() {
        let db = KernelDb::in_memory().unwrap();
        assert!(
            db.get_track("no-such-track").unwrap().is_none(),
            "absent track → None"
        );
    }

    #[test]
    fn get_track_empty_id_is_loud_error() {
        let db = KernelDb::in_memory().unwrap();
        assert!(
            matches!(db.get_track(""), Err(KernelDbError::Validation(_))),
            "empty track_id is corrupt → Validation error, not a silent None"
        );
    }

    #[test]
    fn get_track_zero_period_is_loud_error() {
        // A zero period is a beat that never advances — corrupt stored state.
        let db = KernelDb::in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO tracks (track_id, period_ms, beats_per_phrase)
                 VALUES ('bad', 0, 16)",
                [],
            )
            .unwrap();
        assert!(
            matches!(db.get_track("bad"), Err(KernelDbError::Validation(_))),
            "a 0 period_ms is corrupt → loud Validation error"
        );
    }

    #[test]
    fn get_track_negative_field_is_loud_error() {
        // A negative INTEGER written into SQLite (tampering / bit rot) must not
        // silently cast to a huge u64. Reject it loudly.
        let db = KernelDb::in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO tracks (track_id, period_ms, beats_per_phrase)
                 VALUES ('neg', -1, 16)",
                [],
            )
            .unwrap();
        assert!(
            matches!(db.get_track("neg"), Err(KernelDbError::Validation(_))),
            "a negative period_ms is corrupt → loud Validation error"
        );
    }

    #[test]
    fn track_upsert_updates_in_place() {
        // A tempo change updates one row, not a second — PK is track_id.
        let db = KernelDb::in_memory().unwrap();
        db.upsert_track(&make_track("bass", 500)).unwrap();
        db.upsert_track(&make_track("bass", 250)).unwrap(); // tempo up
        assert_eq!(
            db.get_track("bass").unwrap().unwrap().period_ms,
            250,
            "the second upsert overwrites the first"
        );
        assert_eq!(
            db.list_tracks().unwrap().len(),
            1,
            "one upsert, not two rows"
        );
    }

    #[test]
    fn track_list_and_delete() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);

        db.upsert_track(&make_track("bass", 250)).unwrap();
        db.upsert_track(&make_track("lead", 500)).unwrap();
        assert_eq!(db.list_tracks().unwrap().len(), 2, "two tracks");

        // Attach a context to bass so we can verify cascade.
        let ctx = make_context_row(Some("ctx-cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);
        db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();
        assert_eq!(
            db.list_attachments_for_track("bass").unwrap().len(),
            1,
            "attachment present before delete"
        );

        // Delete bass — should cascade to the attachment.
        assert_eq!(db.delete_track("bass").unwrap(), 1, "one row deleted");
        assert_eq!(db.list_tracks().unwrap().len(), 1, "lead still present");
        assert_eq!(
            db.list_attachments_for_track("bass").unwrap().len(),
            0,
            "attachment cascade-deleted with the track"
        );
    }

    #[test]
    fn track_playhead_and_playing_round_trip() {
        let db = KernelDb::in_memory().unwrap();
        let t = PersistedTrack {
            playhead_tick: Some(1024),
            playing: true,
            ..make_track("bass", 250)
        };
        db.upsert_track(&t).unwrap();
        let got = db.get_track("bass").unwrap().unwrap();
        assert_eq!(got.playhead_tick, Some(1024));
        assert!(got.playing);
    }

    // ── Attachments CRUD ──────────────────────────────────────────────

    #[test]
    fn attachment_upsert_get_round_trip() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        let ctx = make_context_row(Some("att-rt"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let a = PersistedAttachment {
            track_id: "bass".into(),
            context_id: ctx.context_id,
            wakeup_every: 8,
            rotate_every_phrases: Some(4),
            ooda_armed: true,
        };
        db.upsert_attachment(&a).unwrap();
        assert_eq!(
            db.get_attachment("bass", ctx.context_id).unwrap(),
            Some(a),
            "all fields survive write→read"
        );
    }

    #[test]
    fn attachment_absent_is_none() {
        let db = KernelDb::in_memory().unwrap();
        db.upsert_track(&make_track("bass", 250)).unwrap();
        assert!(
            db.get_attachment("bass", ContextId::new()).unwrap().is_none(),
            "absent attachment → None"
        );
    }

    #[test]
    fn attachment_upsert_updates_in_place() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        let ctx = make_context_row(Some("att-up"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 1,
            ..make_attachment("bass", ctx.context_id)
        })
        .unwrap();
        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 8,
            ..make_attachment("bass", ctx.context_id)
        })
        .unwrap();
        assert_eq!(
            db.get_attachment("bass", ctx.context_id).unwrap().unwrap().wakeup_every,
            8,
            "the second upsert overwrites the first (in-place update)"
        );
    }

    #[test]
    fn list_attachments_for_track() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        db.upsert_track(&make_track("lead", 500)).unwrap();

        let ctx1 = make_context_row(Some("att-list-1"));
        let ctx2 = make_context_row(Some("att-list-2"));
        let ctx3 = make_context_row(Some("att-list-3"));
        insert_context_with_doc(&db, &ctx1, ws_id);
        insert_context_with_doc(&db, &ctx2, ws_id);
        insert_context_with_doc(&db, &ctx3, ws_id);

        db.upsert_attachment(&make_attachment("bass", ctx1.context_id)).unwrap();
        db.upsert_attachment(&make_attachment("bass", ctx2.context_id)).unwrap();
        db.upsert_attachment(&make_attachment("lead", ctx3.context_id)).unwrap();

        let bass_atts = db.list_attachments_for_track("bass").unwrap();
        assert_eq!(bass_atts.len(), 2, "two bass attachments");
        assert!(bass_atts.iter().all(|a| a.track_id == "bass"));

        let lead_atts = db.list_attachments_for_track("lead").unwrap();
        assert_eq!(lead_atts.len(), 1, "one lead attachment");
    }

    #[test]
    fn list_attachments_for_context() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        db.upsert_track(&make_track("lead", 500)).unwrap();

        let ctx = make_context_row(Some("att-ctx-list"));
        let other = make_context_row(Some("att-ctx-other"));
        insert_context_with_doc(&db, &ctx, ws_id);
        insert_context_with_doc(&db, &other, ws_id);

        db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();
        db.upsert_attachment(&make_attachment("lead", ctx.context_id)).unwrap();
        db.upsert_attachment(&make_attachment("bass", other.context_id)).unwrap();

        let ctx_atts = db.list_attachments_for_context(ctx.context_id).unwrap();
        assert_eq!(ctx_atts.len(), 2, "ctx attached to two tracks");
        assert!(ctx_atts.iter().all(|a| a.context_id == ctx.context_id));
    }

    #[test]
    fn delete_attachment() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        let ctx = make_context_row(Some("att-del"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.upsert_attachment(&make_attachment("bass", ctx.context_id)).unwrap();
        assert_eq!(
            db.delete_attachment("bass", ctx.context_id).unwrap(),
            1,
            "one row deleted"
        );
        assert!(
            db.get_attachment("bass", ctx.context_id).unwrap().is_none(),
            "attachment gone after delete"
        );
        assert_eq!(
            db.delete_attachment("bass", ctx.context_id).unwrap(),
            0,
            "second delete is a no-op"
        );
    }

    // ── Fork inheritance ──────────────────────────────────────────────

    #[test]
    fn attachments_travel_with_a_fork() {
        // A forked musician must join the SAME tracks as the parent — the track
        // is the durable clock domain identity across the fork-lineage (the
        // rotation page-turn: docs/tracks.md §3). A thin spawn-fork has no label,
        // so without this copy the child would have no track to re-bind on.
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        db.upsert_track(&make_track("lead", 500)).unwrap();

        let parent = make_context_row(Some("musician-parent"));
        insert_context_with_doc(&db, &parent, ws_id);

        // Parent is attached to two tracks with different wakeup divisors.
        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 8,
            rotate_every_phrases: Some(4),
            ooda_armed: true,
            ..make_attachment("bass", parent.context_id)
        })
        .unwrap();
        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 64,
            ..make_attachment("lead", parent.context_id)
        })
        .unwrap();

        // Fork: labelless thin child (spawn shape).
        let mut child = make_context_row(None);
        child.forked_from = Some(parent.context_id);
        db.insert_forked_context(&child, ws_id, parent.context_id).unwrap();

        // Child inherits both attachments with unchanged track_ids.
        let bass_att = db.get_attachment("bass", child.context_id).unwrap().unwrap();
        assert_eq!(bass_att.track_id, "bass");
        assert_eq!(bass_att.wakeup_every, 8);
        assert_eq!(bass_att.rotate_every_phrases, Some(4));
        assert!(bass_att.ooda_armed, "ooda_armed copied from parent");

        let lead_att = db.get_attachment("lead", child.context_id).unwrap().unwrap();
        assert_eq!(lead_att.track_id, "lead");
        assert_eq!(lead_att.wakeup_every, 64);
    }

    #[test]
    fn fork_of_a_non_musician_copies_no_attachments() {
        // A non-musician parent has no attachments; the fork copy is a clean no-op
        // (does not error, does not leave stale rows behind).
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let parent = make_context_row(Some("coder"));
        insert_context_with_doc(&db, &parent, ws_id);

        let mut child = make_context_row(Some("coder-child"));
        child.forked_from = Some(parent.context_id);
        db.insert_forked_context(&child, ws_id, parent.context_id).unwrap();

        assert!(
            db.list_attachments_for_context(child.context_id).unwrap().is_empty(),
            "no parent attachments → none copied (forks of non-musicians are unaffected)"
        );
    }

    #[test]
    fn copy_attachments_for_fork_in_transaction() {
        // Directly tests the tx-level primitive with two source attachments.
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        db.upsert_track(&make_track("bass", 250)).unwrap();
        db.upsert_track(&make_track("lead", 500)).unwrap();

        let source = make_context_row(Some("src-ctx"));
        let child_row = make_context_row(Some("child-ctx"));
        insert_context_with_doc(&db, &source, ws_id);
        insert_context_with_doc(&db, &child_row, ws_id);

        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 4,
            ..make_attachment("bass", source.context_id)
        })
        .unwrap();
        db.upsert_attachment(&PersistedAttachment {
            wakeup_every: 16,
            rotate_every_phrases: Some(2),
            ..make_attachment("lead", source.context_id)
        })
        .unwrap();

        // Run the copy inside a transaction, just like insert_forked_context does.
        let tx = db.conn.transaction().unwrap();
        KernelDb::copy_attachments_for_fork(&tx, source.context_id, child_row.context_id).unwrap();
        tx.commit().unwrap();

        // Child now has the same two attachments, keyed on child's context_id.
        let bass = db.get_attachment("bass", child_row.context_id).unwrap().unwrap();
        assert_eq!(bass.wakeup_every, 4);
        let lead = db.get_attachment("lead", child_row.context_id).unwrap().unwrap();
        assert_eq!(lead.wakeup_every, 16);
        assert_eq!(lead.rotate_every_phrases, Some(2));

        // Source rows are untouched.
        assert!(db.get_attachment("bass", source.context_id).unwrap().is_some());
        assert!(db.get_attachment("lead", source.context_id).unwrap().is_some());
    }

    #[test]
    fn cache_breakpoints_preserve_declaration_order_via_seq() {
        // Insertion order must be preserved — populators (rc scripts)
        // rely on this for the wire-layer's first-write-wins dedupe to
        // produce predictable results.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-order"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let seq0 = db
            .add_cache_breakpoint(
                ctx.context_id,
                &CacheTarget::MessageIndex(0, CacheTtl::Ephemeral),
            )
            .unwrap();
        let seq1 = db
            .add_cache_breakpoint(
                ctx.context_id,
                &CacheTarget::MessageIndex(5, CacheTtl::Ephemeral),
            )
            .unwrap();
        let seq2 = db
            .add_cache_breakpoint(
                ctx.context_id,
                &CacheTarget::MessageIndex(2, CacheTtl::Ephemeral),
            )
            .unwrap();
        assert_eq!(seq0, 0);
        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);

        let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
        assert_eq!(
            bps,
            vec![
                CacheTarget::MessageIndex(0, CacheTtl::Ephemeral),
                CacheTarget::MessageIndex(5, CacheTtl::Ephemeral),
                CacheTarget::MessageIndex(2, CacheTtl::Ephemeral),
            ],
            "list must return breakpoints in insertion order, not sorted by index"
        );
    }

    #[test]
    fn cache_breakpoints_clear_returns_count_and_empties_list() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-clear"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
            .unwrap();
        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::System(CacheTtl::Ephemeral))
            .unwrap();

        assert_eq!(db.clear_cache_breakpoints(ctx.context_id).unwrap(), 2);
        assert!(db.list_cache_breakpoints(ctx.context_id).unwrap().is_empty());

        // Idempotent — clearing again returns 0.
        assert_eq!(db.clear_cache_breakpoints(ctx.context_id).unwrap(), 0);
    }

    #[test]
    fn cache_breakpoints_resume_seq_after_clear() {
        // After clearing, the next add should start back at seq=0.
        // This matters for rc-on-drift scripts that clear-then-rebuild
        // — we don't want sequence numbers to grow unboundedly.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-resume"));
        insert_context_with_doc(&db, &ctx, ws_id);

        for _ in 0..3 {
            db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
                .unwrap();
        }
        db.clear_cache_breakpoints(ctx.context_id).unwrap();

        let new_seq = db
            .add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
            .unwrap();
        assert_eq!(new_seq, 0, "seq restarts at 0 after a full clear");
    }

    #[test]
    fn cache_breakpoints_cascade_delete_on_context() {
        // Context deletion must remove all of its breakpoints (via the
        // FK ON DELETE CASCADE through contexts -> documents).
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-cascade"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
            .unwrap();
        db.add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(3, CacheTtl::Extended),
        )
        .unwrap();

        db.delete_context(ctx.context_id).unwrap();

        assert!(
            db.list_cache_breakpoints(ctx.context_id).unwrap().is_empty(),
            "context delete must cascade to cache_breakpoints"
        );
    }

    #[test]
    fn cache_breakpoints_storage_does_not_enforce_4_cap() {
        // Storage is liberal — populators may add more than 4. The wire
        // layer applies the cap. This test pins the policy choice.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-nocap"));
        insert_context_with_doc(&db, &ctx, ws_id);

        for i in 0..6 {
            db.add_cache_breakpoint(
                ctx.context_id,
                &CacheTarget::MessageIndex(i, CacheTtl::Ephemeral),
            )
            .unwrap();
        }
        assert_eq!(db.list_cache_breakpoints(ctx.context_id).unwrap().len(), 6);
    }

    #[test]
    fn cache_breakpoints_decode_drops_unrecognized_kind() {
        // Forwards-compat: an unknown target_kind in the DB (e.g. from a
        // future schema variant downgraded to this binary) must be
        // silently skipped, not panic the read path.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-future"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Insert one good row and one row with a kind this binary doesn't know.
        db.add_cache_breakpoint(ctx.context_id, &CacheTarget::Tools(CacheTtl::Ephemeral))
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO cache_breakpoints
                     (context_id, seq, target_kind, target_index, ttl)
                 VALUES (?1, 999, 'image_blocks', 4, 'ephemeral')",
                params![blob_param(ctx.context_id.as_bytes())],
            )
            .unwrap();

        let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
        assert_eq!(bps.len(), 1, "unknown kind silently dropped");
        assert_eq!(bps[0], CacheTarget::Tools(CacheTtl::Ephemeral));
    }

    #[test]
    fn cache_breakpoints_decode_drops_unrecognized_ttl() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-future-ttl"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.conn
            .execute(
                "INSERT INTO cache_breakpoints
                     (context_id, seq, target_kind, target_index, ttl)
                 VALUES (?1, 0, 'tools', NULL, 'eternal')",
                params![blob_param(ctx.context_id.as_bytes())],
            )
            .unwrap();

        assert!(db.list_cache_breakpoints(ctx.context_id).unwrap().is_empty());
    }

    #[test]
    fn cache_breakpoints_message_index_zero_round_trips() {
        // Edge case the doc explicitly cares about: rc-on-fork drops
        // MessageIndex(fork_at - 1); when fork_at is 1, that's
        // MessageIndex(0). Make sure index 0 doesn't get confused with
        // SQL NULL or default fallback.
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("cache-idx0"));
        insert_context_with_doc(&db, &ctx, ws_id);

        db.add_cache_breakpoint(
            ctx.context_id,
            &CacheTarget::MessageIndex(0, CacheTtl::Extended),
        )
        .unwrap();
        let bps = db.list_cache_breakpoints(ctx.context_id).unwrap();
        assert_eq!(bps, vec![CacheTarget::MessageIndex(0, CacheTtl::Extended)]);
    }

    // ── 25. Workspace paths with read_only ────────────────────────────

    #[test]
    fn workspace_path_read_only_roundtrip() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "ro-test".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();

        let rw = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src".into(),
            read_only: false,
            created_at: now,
        };
        let ro = WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        };
        db.insert_workspace_path(&rw).unwrap();
        db.insert_workspace_path(&ro).unwrap();

        let paths = db.list_workspace_paths(ws.workspace_id).unwrap();
        assert_eq!(paths.len(), 2);
        // Ordered by path
        assert_eq!(paths[0].path, "/home/user/docs");
        assert!(paths[0].read_only);
        assert_eq!(paths[1].path, "/home/user/src");
        assert!(!paths[1].read_only);
    }

    // ── 26. Fork context config (composite) ──────────────────────────

    #[test]
    fn fork_context_config_full() {
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("fork-src"));
        let tgt = make_context_row(Some("fork-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Set up source with shell config + env vars
        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/home/user/src/kaijutsu".into()),
            updated_at: now_millis() as i64,
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
        let mut db = KernelDb::in_memory().unwrap();
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
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("atomic-src"));
        let tgt = make_context_row(Some("atomic-tgt"));
        insert_context_with_doc(&db, &src, ws_id);
        insert_context_with_doc(&db, &tgt, ws_id);

        // Source has shell config (copied first) AND an env var (copied second).
        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/work".into()),
            updated_at: now_millis() as i64,
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
        let mut db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let src = make_context_row(Some("ifc-src"));
        insert_context_with_doc(&db, &src, ws_id);

        db.upsert_context_shell(&ContextShellRow {
            context_id: src.context_id,
            cwd: Some("/work/kaijutsu".into()),
            updated_at: now_millis() as i64,
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
        db.insert_forked_context(&tgt, ws_id, src.context_id).unwrap();

        // Both the document row and the context row were created.
        assert!(db.get_document(tgt.context_id).unwrap().is_some());
        assert!(db.get_context(tgt.context_id).unwrap().is_some());

        // Shell + env + binding all followed the fork.
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
        let mut db = KernelDb::in_memory().unwrap();
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
        let result = db.insert_forked_context(&tgt, ws_id, src.context_id);
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

    // ── 27. Context workspace paths query ─────────────────────────────

    #[test]
    fn context_workspace_paths_bound() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "project".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();

        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/project".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        })
        .unwrap();

        // Create context with workspace bound
        let mut ctx = make_context_row(Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        let paths = db.context_workspace_paths(ctx.context_id).unwrap();
        let paths = paths.unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn context_workspace_paths_unbound() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("unbound"));
        insert_context_with_doc(&db, &ctx, ws_id);

        let paths = db.context_workspace_paths(ctx.context_id).unwrap();
        assert!(paths.is_none());
    }

    #[test]
    fn get_or_create_kernel_id_stable_across_calls() {
        let db = KernelDb::in_memory().unwrap();
        let id1 = db.kernel_id().unwrap();
        let id2 = db.kernel_id().unwrap();
        assert_eq!(id1, id2, "should return same ID on second call");
    }

    #[test]
    fn get_or_create_kernel_id_fresh_on_empty_db() {
        let db = KernelDb::in_memory().unwrap();
        let id = db.kernel_id().unwrap();
        // Should be a valid UUIDv7 (non-zero)
        assert_ne!(id.as_bytes(), &[0u8; 16]);
    }

    // ── 28. Workspace path permission checking ────────────────────────

    #[test]
    fn check_workspace_path_unbound_context() {
        let db = KernelDb::in_memory().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("unbound"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Unbound context → None (no restriction)
        let result = db
            .check_workspace_path(ctx.context_id, "/anywhere")
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn check_workspace_path_allowed_rw() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Path inside workspace rw path → Some(false)
        let result = db
            .check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/src/main.rs")
            .unwrap();
        assert_eq!(result, Some(false));
    }

    #[test]
    fn check_workspace_path_allowed_ro() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/docs".into(),
            read_only: true,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Path inside workspace ro path → Some(true)
        let result = db
            .check_workspace_path(ctx.context_id, "/home/user/docs/README.md")
            .unwrap();
        assert_eq!(result, Some(true));
    }

    #[test]
    fn check_workspace_path_outside_scope() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // Path outside all workspace paths → Validation error
        let err = db
            .check_workspace_path(ctx.context_id, "/etc/passwd")
            .unwrap_err();
        assert!(matches!(err, KernelDbError::Validation(_)));
    }

    #[test]
    fn check_workspace_path_longest_prefix() {
        let db = KernelDb::in_memory().unwrap();
        let creator = PrincipalId::new();
        let now = now_millis() as i64;

        let ws = WorkspaceRow {
            workspace_id: WorkspaceId::new(),
                        label: "proj".into(),
            description: None,
            created_at: now,
            created_by: creator,
            archived_at: None,
        };
        db.insert_workspace(&ws).unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src".into(),
            read_only: false,
            created_at: now,
        })
        .unwrap();
        db.insert_workspace_path(&WorkspacePathRow {
            workspace_id: ws.workspace_id,
            path: "/home/user/src/kaijutsu/docs".into(),
            read_only: true,
            created_at: now,
        })
        .unwrap();

        let mut ctx = make_context_row(Some("bound"));
        ctx.workspace_id = Some(ws.workspace_id);
        insert_context_with_doc(&db, &ctx, ws.workspace_id);

        // /home/user/src/kaijutsu/src → matches /home/user/src (rw)
        assert_eq!(
            db.check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/src/main.rs")
                .unwrap(),
            Some(false),
        );

        // /home/user/src/kaijutsu/docs/README.md → matches /home/user/src/kaijutsu/docs (ro, longer prefix)
        assert_eq!(
            db.check_workspace_path(ctx.context_id, "/home/user/src/kaijutsu/docs/README.md")
                .unwrap(),
            Some(true),
        );
    }
}
