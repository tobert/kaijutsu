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

/// A context row — superset of `Context` + `ContextHandle`.
#[derive(Debug, Clone)]
pub struct ContextRow {
    pub context_id: ContextId,
    pub label: Option<String>,
    pub system_prompt: Option<String>,
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
    /// Unix-millis of the most recent block append/mutation on this context,
    /// or `None` if never touched since creation (or a pre-migration row).
    /// Stamped by `touch_context_activity`, called from `BlockStore::journal_op`
    /// — the one chokepoint every mutating block op funnels through.
    pub last_activity_at: Option<i64>,
    /// Unix-millis of the explicit ring-0 ("active") promote, or `None` if
    /// never promoted. First-write-wins: re-promoting an already-promoted
    /// context is a no-op that preserves this timestamp, so ring-0 order
    /// stays append-stable. Mutually exclusive with `demoted_at` — setting
    /// one clears the other. Cleared by `conclude_context` (gives up the
    /// ring-0 seat) and by `archive_context`.
    pub promoted_at: Option<i64>,
    /// Unix-millis of the explicit push to the demoted ring, or `None` if
    /// never demoted. Mutually exclusive with `promoted_at`. Cleared by
    /// `archive_context`. See the demote ladder in `demote_context`.
    pub demoted_at: Option<i64>,
    /// Unix-millis of the explicit "suspend activity" flag, or `None` if not
    /// paused. Design-only for now: persisted and exposed on the wire, but
    /// not yet wired to any behavioral gating. Intended semantics once wired:
    /// a paused context receives no beat/OODA wakeups (seam: hyoushigi
    /// attachment wakeup fire) and rejects turn-starts loudly (seam: kernel
    /// turn-start).
    pub paused_at: Option<i64>,
    /// Advisory hostname the registering client self-reported (`gethostname`
    /// on the kaijutsu-mcp side today), or `None` when unknown — an old
    /// client that never called `setContextOriginHost`, a creation path with
    /// no client to ask (genesis bootstrap, fork), or a pre-migration row.
    /// Set ONCE, right after creation, via `KernelDb::set_origin_host` (never
    /// baked into `write_context`'s own INSERT) — mirrors `created_by`: an
    /// origin fact, not a "last seen from" one, so a later reconnect from a
    /// different machine does NOT overwrite it. Shared-trust model: this is
    /// observability, not auth — the server trusts the client's self-report.
    /// Rendered as `"-"` when `None` (`kj context info`,
    /// `kj context list`).
    pub origin_host: Option<String>,
    /// The character whose performance this context is, or `None` for a
    /// context with no context character (a score context, a file document,
    /// the handoff log). Read from the context's context character; no
    /// column stores it, and `write_context` refuses a row that sets it. See
    /// `docs/senpai-kohai.md`, "The model".
    pub played_by: Option<PrincipalId>,
}

/// The columns of a pre-instance `contexts` row that `senpai-edges-v1` reads.
struct OldContext {
    context_id: ContextId,
    label: Option<String>,
    created_at: i64,
    forked_from: Option<ContextId>,
    archived_at: Option<i64>,
    played_by: Option<PrincipalId>,
}

/// One explicit model-drive epoch that may receive automatic continuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuationOrigin {
    pub epoch: i64,
    pub opened_at: i64,
}

impl ContextRow {
    /// Whether this context is archived, and therefore inert: it runs
    /// nothing and answers nothing.
    ///
    /// `archived_at` is authoritative per `ContextState`'s own doc comment,
    /// so it is checked ahead of the enum. The kernel writes both together —
    /// `archive_context` stamps the timestamp and the state, and resurrection
    /// clears both — so this reads two fields that agree. It stays one
    /// predicate anyway: a caller that checks a single field is a caller that
    /// treats a dead context as live if they ever part again.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some() || self.context_state == ContextState::Archived
    }

    /// Whether this is the ROOT console: the `console` context labeled
    /// `ROOT` (`docs/character.md`, "The root console and its shell").
    pub fn is_root_console(&self) -> bool {
        self.context_type == kaijutsu_types::CONSOLE_CONTEXT_TYPE && self.label.as_deref() == Some("ROOT")
    }

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

/// Hard cap on the number of contexts holding a ring-0 seat
/// (`promoted_at IS NOT NULL AND archived_at IS NULL`) at once. Ring 0
/// renders exactly this many slots, addressed by digit hotkeys 0-9, and is
/// Amy's hand-curated seat row: seats must never appear or vanish without an
/// explicit act, so [`KernelDb::promote_context`] refuses loudly instead of
/// bumping the oldest occupant when the ring is full. Derived from the
/// canonical [`kaijutsu_types::RING_SLOTS`] (the same 10 the app's ring
/// layout seats).
pub const ACTIVE_RING_CAPACITY: i64 = kaijutsu_types::RING_SLOTS as i64;

/// Where a review route starts (`docs/senpai-kohai.md`, "Routing").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFrom {
    /// An ask raised in this existing context: routing starts at its senpai.
    Context(ContextId),
    /// A context not yet written whose senpai will be this context (a create
    /// before the insert): routing starts at that senpai.
    Senpai(ContextId),
}

/// An act the one authority rule governs (`docs/senpai-kohai.md`, "Authority").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernedAct {
    /// `kj context set <ctx> --egress-allow|--egress-deny`.
    Egress,
    /// `kj context move`.
    Move,
    /// `kj ledger escalate` on an ask raised in the context.
    Escalate,
    /// `kj context rotate` (its own performer may also rotate it).
    Rotate,
    /// `kj context archive`.
    Archive,
}

impl GovernedAct {
    pub const ALL: [GovernedAct; 5] = [Self::Egress, Self::Move, Self::Escalate, Self::Rotate, Self::Archive];

    /// The act, as the refusal names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Egress => "change its egress list",
            Self::Move => "move it to another senpai",
            Self::Escalate => "escalate its asks",
            Self::Rotate => "rotate it",
            Self::Archive => "archive it",
        }
    }
}

/// Which half of the authority rule matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    /// The actor holds the context's senpai.
    SenpaiHolder,
    /// The actor is the context's lineage root.
    LineageRoot,
    /// The actor is a live root character archiving a context with no
    /// context character, which has no senpai and no lineage root.
    LiveRoot,
}

/// One request to [`KernelDb::route_review`].
#[derive(Debug, Clone, Copy)]
pub struct ReviewQuery {
    pub from: ReviewFrom,
    /// The character performing; it never reviews its own work.
    pub actor: PrincipalId,
    /// Climb past the context held by this reviewer before landing: an
    /// escalation, or the reviewer-context chain, asks for the holder above
    /// the one it already has. The reviewer must hold a senpai on the chain.
    pub past: Option<PrincipalId>,
    /// Whether a live root `actor` confirms its own statement when the climb
    /// reaches a context with no senpai. Routing an ask does; an escalation
    /// or a chain walk instead refuses there, naming the context.
    pub confirm_at_top: bool,
}

impl ReviewQuery {
    /// The reviewer of an ask `actor` raises in `context`.
    pub fn ask(context: ContextId, actor: PrincipalId) -> Self {
        Self { from: ReviewFrom::Context(context), actor, past: None, confirm_at_top: true }
    }

    /// The reviewer of an ask `actor` would raise in a context whose senpai is `senpai`.
    pub fn under(senpai: ContextId, actor: PrincipalId) -> Self {
        Self { from: ReviewFrom::Senpai(senpai), actor, past: None, confirm_at_top: true }
    }

    /// The holder above `reviewer` on the senpai chain: climb past the
    /// context `reviewer` holds, and refuse rather than self-confirm at the top.
    pub fn above(mut self, reviewer: PrincipalId) -> Self {
        self.past = Some(reviewer);
        self.confirm_at_top = false;
        self
    }
}

/// Outcome of [`KernelDb::route_review`]. Distinct from a
/// `KernelDbResult` error: an exhausted walk for a root actor is an
/// expected terminal state, not a ledger fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveReviewer {
    /// A live character, distinct from the acting one, reviews its work.
    Assigned(PrincipalId),
    /// Every resolution layer is exhausted for a live root actor: no
    /// explicit override or delegation applies, and no context at or above
    /// this one has a live responsible character other than the actor. The
    /// actor is at its own root, so it reviews its own work: the ask is
    /// raised with the actor as reviewer and only the actor may answer it
    /// (`docs/approval-identity.md`).
    SelfConfirmation(PrincipalId),
    /// A non-root actor acts in a root context the live root holds, and that
    /// root reviews it. The context has no senpai: the holder is the root
    /// itself.
    TopHolder(PrincipalId),
}

impl EffectiveReviewer {
    /// The reviewer to snapshot on an ask, either way. A self-confirmation
    /// names the actor.
    pub fn reviewer(self) -> PrincipalId {
        match self {
            Self::Assigned(reviewer) | Self::TopHolder(reviewer) => reviewer,
            Self::SelfConfirmation(actor) => actor,
        }
    }
}

/// Outcome of one step on the demote ladder — see
/// [`KernelDb::demote_context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoteOutcome {
    /// Was promoted (ring 0) → `promoted_at` cleared, back to automatic placement.
    Unpromoted,
    /// Was neither promoted nor demoted → `demoted_at` set (pushed to the demoted ring).
    Demoted,
    /// Was already demoted → archived (one step further outward, past the rim).
    Archived,
}

/// Outcome of a promote — see [`KernelDb::promote_context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// Newly seated in ring 0 (`promoted_at` stamped, `demoted_at` cleared).
    Promoted,
    /// Already held a seat — no-op, original timestamp preserved.
    AlreadyPromoted,
    /// Was archived — resurrected: promote is the resurrection door (the
    /// archive is memory to drift back from, not trash). `archived_at`
    /// cleared, a FRESH `promoted_at` stamped (the old seat was surrendered
    /// at archive time), `demoted_at` cleared. `concluded` reports whether
    /// the row is still concluded — placement is orthogonal to conclusion
    /// ("concluded but seated for review" is a legit state), and callers
    /// need it to restore the right DriftRouter lifecycle state.
    Resurrected {
        /// The resurrected row's `concluded_at` is still set.
        concluded: bool,
    },
}

/// A document row — the document content layer.
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
///
/// `state` is the whole document, so there is no separate text column: the
/// text is what decoding `state` produces. A reader that wants only the text
/// still decodes, which is the cost of having one copy instead of two that
/// can disagree.
#[derive(Debug, Clone)]
pub struct DocSnapshotRow {
    pub document_id: ContextId,
    pub seq: i64,
    pub version: i64,
    pub state: Vec<u8>,
    pub created_at: i64,
}

/// A drift edge row: content moved from `source_id` to `target_id`.
#[derive(Debug, Clone)]
pub struct ContextEdgeRow {
    pub edge_id: uuid::Uuid,
    pub source_id: ContextId,
    pub target_id: ContextId,
    pub metadata: Option<String>,
    pub created_at: i64,
}

// ============================================================================
// Schema
// ============================================================================

/// Record the model's ToolCall block that raised `request`, in the
/// transaction that creates the ask. The call must belong to the ask's context.
pub(crate) fn record_approval_tool_call(
    conn: &Connection, request: &str, context: ContextId, call: &BlockId,
) -> KernelDbResult<()> {
    if call.context_id != context {
        return Err(KernelDbError::Validation(format!(
            "tool call {} belongs to context {}, not the ask's context {context}", call.to_key(), call.context_id)));
    }
    conn.execute("INSERT INTO approval_tool_calls(request_id, block_id) VALUES (?1, ?2)",
        rusqlite::params![request, call.to_key()])?;
    Ok(())
}

/// Record the held drift block a drift ask answers for, in the ask's
/// transaction.
pub(crate) fn record_approval_drift_block(conn: &Connection, request: &str, block: &BlockId) -> KernelDbResult<()> {
    conn.execute("INSERT INTO approval_drift_blocks(request_id, block_id) VALUES (?1, ?2)",
        rusqlite::params![request, block.to_key()])?;
    Ok(())
}

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
    /// Abandon every ask that has not reached a terminal state, once at
    /// kernel cold start. Returns how many rows moved.
    ///
    /// The public face of `approval_ledger::decide::abandon_unresolved_on_restart`
    /// for callers outside this crate: the ledger's whole API borrows a
    /// `Connection`, and [`Self::conn_for_ledger`] is `pub(crate)` on
    /// purpose, so the server crate reaches the sweep through here rather
    /// than through the raw connection.
    ///
    /// `reason` is displayed by `kj ledger show`, so it should name the
    /// condition without claiming execution never occurred: result reviews
    /// follow execution. See `docs/gate-resume.md`.
    pub fn abandon_unresolved_asks_on_restart(&self, reason: &str) -> KernelDbResult<usize> {
        Ok(approval_ledger::decide::abandon_unresolved_on_restart(
            self.conn_for_ledger(),
            reason,
        )?)
    }

    /// Link the ask's original pair and retain its execution receipt together.
    /// Executable asks reuse an existing receipt or adopt the supplied pair.
    /// The caller validates the pair before linking; context and performer
    /// must match the ask. A different pair or owner cannot replace a link.
    ///
    /// Joins an existing transaction; its caller must roll back on error.
    /// Turn pairs need a model notification after settlement; Session pairs
    /// are read directly by their connected player.
    pub fn link_ask_blocks(
        &self,
        request_id: &str,
        command_block_id: &BlockId,
        output_block_id: &BlockId,
        owner: approval_ledger::types::PairOwner,
    ) -> KernelDbResult<()> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.link_ask_blocks(request_id, command_block_id, output_block_id, owner));
        }
        crate::shell_operations::link_approval_pair(self, request_id, command_block_id, output_block_id, owner)
    }

    /// The free-variable env snapshot recorded on this ask
    /// (`kj::env_snapshot::free_variable_values`, captured at
    /// `kj::gate::build_ask` time), in the order it was captured. Empty for
    /// an ask with no free variables, an unknown ask, or an origin that
    /// records no snapshot. `docs/gate-resume.md`.
    pub fn ask_env(&self, request_id: &str) -> KernelDbResult<Vec<approval_ledger::types::AskEnvRow>> {
        Ok(approval_ledger::ask::load_ask_env(self.conn_for_ledger(), request_id)?)
    }

    /// Open a new continuation epoch for an explicit model drive.
    pub fn begin_continuation(&self, context_id: ContextId, opened_at: i64) -> KernelDbResult<ContinuationOrigin> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.begin_continuation(context_id, opened_at));
        }
        let previous: Option<i64> = self.conn.query_row(
            "SELECT epoch FROM context_continuations WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |row| row.get(0),
        ).optional()?;
        let epoch = previous.unwrap_or(0).checked_add(1)
            .ok_or_else(|| KernelDbError::Validation("continuation epoch overflow".into()))?;
        self.conn.execute(
            "INSERT INTO context_continuations (context_id, epoch, opened_at, last_request_at, yielded_at, signed_off_at, interrupted_at, interrupted_by)
             VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, NULL)
             ON CONFLICT(context_id) DO UPDATE SET epoch = excluded.epoch, opened_at = excluded.opened_at,
                 last_request_at = NULL, yielded_at = NULL, signed_off_at = NULL, interrupted_at = NULL, interrupted_by = NULL",
            params![blob_param(context_id.as_bytes()), epoch, opened_at],
        )?;
        Ok(ContinuationOrigin { epoch, opened_at })
    }

    /// Stamp an actual provider inference request for this open epoch.
    pub fn record_continuation_request(&self, context_id: ContextId, epoch: i64, requested_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.record_continuation_request(context_id, epoch, requested_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET last_request_at = ?1
             WHERE context_id = ?2 AND epoch = ?3 AND signed_off_at IS NULL",
            params![requested_at, blob_param(context_id.as_bytes()), epoch],
        )? != 0)
    }

    /// Admit inference from an accepted turn unless its performer was reassigned.
    /// Signoff and newer explicit drives only close automatic resumption; an
    /// accepted request can finish without refreshing a closed or newer window.
    pub(crate) fn admit_inference_request(&self, context_id: ContextId, epoch: i64, requested_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.admit_inference_request(context_id, epoch, requested_at));
        }
        let valid: bool = self.conn.query_row(
            "SELECT ?2 > invalidated_through AND ?2 <= epoch FROM context_continuations WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes()), epoch], |row| row.get(0),
        )?;
        if !valid { return Ok(false); }
        self.record_continuation_request(context_id, epoch, requested_at)?;
        Ok(true)
    }

    /// Return the current unsigned continuation epoch, if this context has one.
    pub fn continuation_epoch(&self, context_id: ContextId) -> KernelDbResult<Option<i64>> {
        self.conn.query_row(
            "SELECT epoch FROM context_continuations WHERE context_id = ?1 AND signed_off_at IS NULL",
            params![blob_param(context_id.as_bytes())],
            |row| row.get(0),
        ).optional().map_err(Into::into)
    }

    /// Mark the matching epoch as yielded without extending its window.
    pub fn record_continuation_yield(&self, context_id: ContextId, epoch: i64, yielded_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.record_continuation_yield(context_id, epoch, yielded_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET yielded_at = ?1
             WHERE context_id = ?2 AND epoch = ?3 AND signed_off_at IS NULL",
            params![yielded_at, blob_param(context_id.as_bytes()), epoch],
        )? != 0)
    }

    /// End the current continuation epoch through an explicit signoff.
    pub fn sign_off_continuation(&self, context_id: ContextId, signed_off_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.sign_off_continuation(context_id, signed_off_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET signed_off_at = ?1
             WHERE context_id = ?2 AND signed_off_at IS NULL",
            params![signed_off_at, blob_param(context_id.as_bytes())],
        )? != 0)
    }

    /// Record that this context's open continuation epoch was interrupted
    /// (`kj interrupt` or RPC `interruptContext`), closing it to automatic
    /// resume. Distinct from [`Self::sign_off_continuation`]: that is the
    /// performer's own handoff close, this is a reviewer or director
    /// stopping the loop from outside. Idempotent and DB-first — a caller
    /// records this before touching the running turn, so a completion
    /// racing the interrupt can never claim a resume. Returns `false`
    /// without writing when there is no open epoch (none opened, already
    /// signed off) or one is already interrupted.
    pub fn record_continuation_interrupt(
        &self,
        context_id: ContextId,
        interrupted_by: PrincipalId,
        interrupted_at: i64,
    ) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.record_continuation_interrupt(context_id, interrupted_by, interrupted_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET interrupted_at = ?1, interrupted_by = ?2
             WHERE context_id = ?3 AND signed_off_at IS NULL AND interrupted_at IS NULL",
            params![interrupted_at, blob_param(interrupted_by.as_bytes()), blob_param(context_id.as_bytes())],
        )? != 0)
    }

    /// Who interrupted this context's open continuation epoch, and when —
    /// `None` when it was never interrupted (or has no continuation row).
    pub fn continuation_interrupt(&self, context_id: ContextId) -> KernelDbResult<Option<(i64, PrincipalId)>> {
        self.conn.query_row(
            "SELECT interrupted_at, interrupted_by FROM context_continuations
             WHERE context_id = ?1 AND interrupted_at IS NOT NULL",
            params![blob_param(context_id.as_bytes())],
            |row| Ok((row.get(0)?, read_principal_id(row, 1)?)),
        ).optional().map_err(Into::into)
    }

    /// Whether a settled operation may automatically request another turn.
    /// The epoch must have yielded, but the deadline is its last provider
    /// inference request rather than the yield itself. An interrupted epoch
    /// (`interrupted_at` set) never resumes automatically, even within the
    /// window — only an explicit new epoch (`begin_continuation`) reopens it.
    pub fn automatic_resume_allowed(
        &self,
        context_id: ContextId,
        epoch: i64,
        now: i64,
        window_ms: i64,
    ) -> KernelDbResult<bool> {
        let row: Option<(i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> = self.conn.query_row(
            "SELECT epoch, last_request_at, yielded_at, signed_off_at, interrupted_at FROM context_continuations WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).optional()?;
        let Some((current, Some(last_request_at), Some(_), None, None)) = row else { return Ok(false) };
        Ok(current == epoch && now.saturating_sub(last_request_at) <= window_ms)
    }

    /// Claim one automatic resume from an eligible yielded continuation.
    pub fn claim_automatic_resume(&self, context_id: ContextId, epoch: i64, now: i64, window_ms: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.claim_automatic_resume(context_id, epoch, now, window_ms));
        }
        if !self.automatic_resume_allowed(context_id, epoch, now, window_ms)? {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE context_continuations SET yielded_at = NULL WHERE context_id = ?1 AND epoch = ?2",
            params![blob_param(context_id.as_bytes()), epoch],
        )?;
        Ok(true)
    }

    /// The character that answers for `context`: the principal of its
    /// context character (`docs/senpai-kohai.md`, "The model"). `None` when
    /// the context has none.
    pub fn context_holder(&self, context: ContextId) -> KernelDbResult<Option<PrincipalId>> {
        Ok(self.context_character(context)?.map(|cc| cc.principal_id))
    }

    /// `label (short-id)`, for messages that name a context.
    fn context_display(row: &ContextRow) -> String {
        match &row.label {
            Some(label) => format!("{label} ({})", row.context_id.short()),
            None => row.context_id.short(),
        }
    }

    /// The senpai context of `context`, or `None` for a top or a context with
    /// no context character.
    pub fn senpai_of(&self, context: ContextId) -> KernelDbResult<Option<ContextId>> {
        Ok(self.context_character(context)?.and_then(|cc| cc.senpai_ctx))
    }

    /// The live kohai of `context`: the contexts whose senpai it is, oldest
    /// first. A context with no kohai (or no context character) lists none.
    pub fn list_kohai(&self, context: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut kohai = Vec::new();
        for id in self.kohai_of(context)? {
            kohai.push(self.get_context(id)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("kohai {} of context {}", id.short(), context.short()))
            })?);
        }
        Ok(kohai)
    }

    /// Route a review: the one resolver behind every reviewer decision
    /// (`docs/senpai-kohai.md`, "Routing").
    ///
    /// Let K be the ask's context. The reviewer is the character of K's
    /// senpai context when it is live and is neither the actor nor the
    /// reviewer `past`; a character that is, is climbed past to its own
    /// senpai. A retired character refuses by name: the climb never
    /// substitutes another. At a context with no senpai (a root console)
    /// the live root character that holds it confirms its own statement
    /// when `confirm_at_top`; otherwise the climb refuses, naming the actor
    /// and the context. A context with no context character refuses.
    ///
    /// A missing context, a senpai with no context character and a senpai
    /// cycle are corruption and are errors, never an exhausted climb.
    pub fn route_review(&self, query: ReviewQuery) -> KernelDbResult<EffectiveReviewer> {
        let ReviewQuery { from, actor, past, confirm_at_top } = query;
        let mut passed = past.is_none();
        let (origin, mut below, mut senpai): (Option<ContextId>, Option<ContextCharacterRow>, Option<ContextId>) = match from {
            ReviewFrom::Context(context) => {
                if self.get_context(context)?.is_none() {
                    return Err(KernelDbError::NotFound(format!(
                        "context {} has no row to resolve a reviewer from", context.short()
                    )));
                }
                let character = self.require_context_character(context)?;
                let senpai = character.senpai_ctx;
                (Some(context), Some(character), senpai)
            }
            ReviewFrom::Senpai(context) => (None, None, Some(context)),
        };
        // The performer of the context the ask is raised in never reviews it,
        // whoever the actor is: a fork's performer is its source's holder.
        let performer = below.as_ref().map(|row| row.principal_id);
        let mut visited = HashSet::new();
        if let Some(row) = &below {
            visited.insert(row.context_id);
        }
        loop {
            let Some(senpai_id) = senpai else {
                let at = below.as_ref().expect("a climb with no senpai started at a context");
                if !passed {
                    let who = past.map_or_else(String::new, |p| self.name_for(p));
                    return Err(KernelDbError::NoReviewerAbove(format!(
                        "{who} holds no senpai context on the chain above {}; it was assigned by hand, so name the next reviewer explicitly",
                        self.context_name(at.context_id)
                    )));
                }
                return self.at_top(at, origin, actor, confirm_at_top);
            };
            if !visited.insert(senpai_id) {
                return Err(KernelDbError::Corrupt(format!(
                    "context {} is its own senpai ancestor; the senpai chain has a cycle",
                    self.context_name(senpai_id)
                )));
            }
            let above = self.context_character(senpai_id)?.ok_or_else(|| KernelDbError::NotFound(format!(
                "senpai {} has no context character, so the climb cannot continue", self.context_name(senpai_id)
            )))?;
            let under = below.as_ref().map_or_else(|| "the new context".to_string(), |b| self.context_name(b.context_id));
            let holder = above.principal_id;
            if !passed {
                // Still climbing to the context the current reviewer holds.
                passed = Some(holder) == past;
                senpai = above.senpai_ctx;
                below = Some(above);
                continue;
            }
            if holder == actor || Some(holder) == past || Some(holder) == performer {
                senpai = above.senpai_ctx;
                below = Some(above);
                continue;
            }
            return match self.get_character(holder)? {
                Some(sheet) if sheet.retired_at.is_none() => Ok(EffectiveReviewer::Assigned(holder)),
                Some(sheet) => Err(KernelDbError::Validation(format!(
                    "{} is {}, the senpai of {under}, and is retired; routing does not pass over a retired character: \
                     move {under} to another senpai with `kj context move`",
                    sheet.name, self.context_name(above.context_id)
                ))),
                None => Err(KernelDbError::Corrupt(format!(
                    "the character of {}, the senpai of {under}, has no character sheet",
                    self.context_name(above.context_id)
                ))),
            };
        }
    }

    /// The climb reached `at`, a context with no senpai. `origin` is the
    /// context the ask was raised in, when there is one.
    fn at_top(&self, at: &ContextCharacterRow, origin: Option<ContextId>, actor: PrincipalId, confirm_at_top: bool) -> KernelDbResult<EffectiveReviewer> {
        let name = origin.map_or_else(|| self.context_name(at.context_id), |context| self.context_name(context));
        if !confirm_at_top {
            return Err(KernelDbError::NoReviewerAbove(format!(
                "context {} has no senpai, so nobody is above it",
                self.context_name(at.context_id)
            )));
        }
        match self.get_character(actor)? {
            Some(sheet) if sheet.root && sheet.retired_at.is_none() && at.principal_id == actor => {
                Ok(EffectiveReviewer::SelfConfirmation(actor))
            }
            Some(sheet) if !sheet.root
                && self.get_character(at.principal_id)?.is_some_and(|holder| holder.root && holder.retired_at.is_none()) =>
            {
                Ok(EffectiveReviewer::TopHolder(at.principal_id))
            }
            sheet => {
                let who = sheet.map_or_else(|| actor.short(), |sheet| sheet.name);
                Err(KernelDbError::Validation(format!(
                    "nobody reviews {who} in context {name}: {} has no senpai above it, and only the live \
                     root character holding a root console confirms its own statements; give the context a senpai \
                     with `kj context move`",
                    self.context_name(at.context_id)
                )))
            }
        }
    }

    /// The root character at the top of `context_id`'s senpai chain: the
    /// character of the context the chain ends at. The inner `Err` says why
    /// the lineage has no live root; the outer `Err` is a fault.
    fn lineage_root_of_context(&self, context_id: ContextId) -> KernelDbResult<Result<PrincipalId, String>> {
        let top = self.lineage_top(context_id)?;
        let top_name = self.context_name(top.context_id);
        Ok(match self.get_character(top.principal_id)? {
            Some(sheet) if !sheet.root => Err(format!(
                "{} holds {top_name} at the top of this senpai chain and is not a root character",
                sheet.name
            )),
            Some(sheet) if sheet.retired_at.is_some() => Err(format!(
                "root character {} is retired, so {top_name} has no lineage root",
                sheet.name
            )),
            Some(_) => Ok(top.principal_id),
            None => Err(format!("the character of {top_name} has no character sheet")),
        })
    }

    /// The lineage root of `context_id`, or the reason it has none.
    fn lineage_root_or_reason(&self, context_id: ContextId) -> KernelDbResult<Result<PrincipalId, String>> {
        if self.get_context(context_id)?.is_none() {
            return Err(KernelDbError::NotFound(format!("context {}", context_id.short())));
        }
        match self.context_character(context_id)? {
            Some(_) => self.lineage_root_of_context(context_id),
            None => Ok(Err(format!(
                "context {} has no context character",
                self.context_name(context_id)
            ))),
        }
    }

    /// The root character at the top of `context_id`'s senpai chain. It
    /// holds routing authority over every context in that lineage
    /// (`docs/approval-identity.md`). A top held by a non-root or a retired
    /// root, a context with no context character, and a broken chain all
    /// refuse.
    pub fn lineage_root(&self, context_id: ContextId) -> KernelDbResult<PrincipalId> {
        match self.lineage_root_or_reason(context_id)? {
            Ok(root) => Ok(root),
            Err(reason) => Err(KernelDbError::Validation(reason)),
        }
    }

    /// The lineage root, or `None` when the lineage legitimately has no live
    /// root: the context has no context character, the top is not a root, is
    /// retired, or has no sheet, and when `context_id` itself has no row
    /// (`approvals.context_id` has no foreign key, so an ask may outlive its
    /// context). A fault (a missing ancestor, a senpai cycle, a database
    /// error) is an `Err`, never `None`.
    pub fn live_lineage_root(&self, context_id: ContextId) -> KernelDbResult<Option<PrincipalId>> {
        if self.get_context(context_id)?.is_none() {
            return Ok(None);
        }
        Ok(self.lineage_root_or_reason(context_id)?.ok())
    }

    /// The one authority rule (`docs/senpai-kohai.md`, "Authority"): the
    /// character of `target`'s senpai, or the lineage root, may perform `act`
    /// on it. The inner `Err` is the refusal, naming the rule, the senpai's
    /// character and the root; the outer `Err` is a fault (a missing row, a
    /// senpai cycle). The context's own character is refused here for every
    /// act; `kj context rotate` admits it before this check.
    pub fn context_authority(
        &self,
        target: ContextId,
        actor: PrincipalId,
        act: GovernedAct,
    ) -> KernelDbResult<Result<Authority, String>> {
        let row = self.get_context(target)?.ok_or_else(|| KernelDbError::NotFound(format!(
            "context {} has no row to check authority over", target.short()
        )))?;
        let Some(character) = self.context_character(target)? else {
            if act == GovernedAct::Archive
                && self.get_character(actor)?.is_some_and(|sheet| sheet.root && sheet.retired_at.is_none())
            {
                return Ok(Ok(Authority::LiveRoot));
            }
            let only_root = if act == GovernedAct::Archive { "; only a live root character may archive it" } else { "" };
            return Ok(Err(format!(
                "{} has no context character, so it has no senpai or lineage root to {}{only_root}",
                Self::context_display(&row), act.as_str()
            )));
        };
        let holder = match character.senpai_ctx {
            Some(senpai) => Some(self.context_character(senpai)?.ok_or_else(|| KernelDbError::NotFound(format!(
                "senpai {} has no context character", self.context_name(senpai)
            )))?.principal_id),
            None => None,
        };
        if holder == Some(actor) {
            return Ok(Ok(Authority::SenpaiHolder));
        }
        let root = self.lineage_root_of_context(target)?;
        if root.as_ref() == Ok(&actor) {
            return Ok(Ok(Authority::LineageRoot));
        }
        let holder_name = holder.map_or_else(|| "none".to_string(), |h| self.name_for(h));
        let root_name = match root {
            Ok(root) => self.name_for(root),
            Err(reason) => format!("none: {reason}"),
        };
        Ok(Err(format!(
            "only {}'s senpai ({holder_name}) or its lineage root ({root_name}) may {}",
            self.context_name(target), act.as_str()
        )))
    }

    /// Whether `actor` may place a context under `senpai`: it is the
    /// character of `senpai`, or `senpai`'s lineage root
    /// (`docs/senpai-kohai.md`, "Creating, forking, rotating"). The inner
    /// `Err` is the refusal.
    pub fn may_place_under(&self, senpai: ContextId, actor: PrincipalId) -> KernelDbResult<Result<Authority, String>> {
        let row = self.context_character(senpai)?.ok_or_else(|| KernelDbError::NotFound(format!(
            "senpai {} has no context character", self.context_name(senpai)
        )))?;
        if row.principal_id == actor {
            return Ok(Ok(Authority::SenpaiHolder));
        }
        let root = self.lineage_root_of_context(senpai)?;
        if root.as_ref() == Ok(&actor) {
            return Ok(Ok(Authority::LineageRoot));
        }
        let holder_name = self.name_for(row.principal_id);
        let root_name = match root {
            Ok(root) => self.name_for(root),
            Err(reason) => format!("none: {reason}"),
        };
        Ok(Err(format!(
            "only the holder of {} ({holder_name}) or its lineage root ({root_name}) may place a context under it",
            self.context_name(senpai)
        )))
    }

    /// The public face of `approval_ledger::ask::list_pending` — the asks
    /// waiting on a human right now.
    ///
    /// `claimed` rows are excluded, matching the queue `kj ledger list`
    /// shows: an answerer is already working them.
    pub fn list_pending_asks(&self) -> KernelDbResult<Vec<approval_ledger::types::ApprovalRow>> {
        Ok(approval_ledger::ask::list_pending(self.conn_for_ledger())?)
    }

    /// Uncollected human answers for the execution/resume driver.
    /// Result reviews are consumed by their retained outcome owner.
    ///
    /// Runtime delivery reads through this API; the ledger owns its schema.
    /// See `docs/gate-resume.md`.
    pub fn undelivered_answers(
        &self,
    ) -> KernelDbResult<Vec<approval_ledger::ask::UndeliveredAnswer>> {
        Ok(approval_ledger::ask::undelivered_answers(
            self.conn_for_ledger(),
        )?)
    }

    /// Record the caller's publication contract before an ask becomes visible.
    #[cfg(test)]
    pub(crate) fn create_approval_ask(&self, ask: &approval_ledger::types::NewAsk, publishes_pair: bool) -> KernelDbResult<String> {
        self.create_approval_ask_recorded(ask, publishes_pair, |_, _| Ok(()))
    }

    /// Commit related caller state before publishing an ask. The callback must
    /// use this connection and must not acquire the database guard again.
    pub(crate) fn create_approval_ask_recorded(
        &self, ask: &approval_ledger::types::NewAsk, publishes_pair: bool,
        record: impl FnOnce(&rusqlite::Connection, &str) -> KernelDbResult<()>,
    ) -> KernelDbResult<String> {
        approval_ledger::ask::create_ask_recorded(self.conn_for_ledger(), ask, |conn, request| {
            if publishes_pair {
                conn.execute("INSERT INTO approval_pair_handoffs(request_id) VALUES (?1)", [request])?;
            }
            record(conn, request)
        })
    }

    /// The model's ToolCall block recorded with this ask, if a model call raised it.
    pub(crate) fn approval_tool_call(&self, request: &str) -> KernelDbResult<Option<BlockId>> {
        let key: Option<String> = self.conn.query_row("SELECT block_id FROM approval_tool_calls WHERE request_id=?1",
            [request], |row| row.get(0)).optional()?;
        key.map(|key| BlockId::from_key(&key)
            .ok_or_else(|| KernelDbError::Validation(format!("ask {request} records a malformed tool call block {key}"))))
            .transpose()
    }

    /// The drift ask recorded with a held drift block.
    pub(crate) fn approval_for_drift_block(&self, block: &BlockId) -> KernelDbResult<Option<String>> {
        Ok(self.conn.query_row("SELECT request_id FROM approval_drift_blocks WHERE block_id=?1",
            [block.to_key()], |row| row.get(0)).optional()?)
    }

    /// The held drift block recorded with a drift ask.
    pub(crate) fn approval_drift_block(&self, request: &str) -> KernelDbResult<Option<BlockId>> {
        let key: Option<String> = self.conn.query_row("SELECT block_id FROM approval_drift_blocks WHERE request_id=?1",
            [request], |row| row.get(0)).optional()?;
        key.map(|key| BlockId::from_key(&key)
            .ok_or_else(|| KernelDbError::Validation(format!("ask {request} records a malformed drift block {key}"))))
            .transpose()
    }

    pub(crate) fn approval_pair_expected(&self, request: &str) -> KernelDbResult<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM approval_pair_handoffs WHERE request_id=?1)",
            [request], |row| row.get(0))?)
    }

    /// A caller that promised a pair releases delivery with its Waiting result.
    /// Call with the same database guard used to admit approval execution.
    pub(crate) fn approval_pair_ready(&self, request: &str) -> KernelDbResult<bool> {
        Ok(self.conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM approval_pair_handoffs WHERE request_id=?1)
             OR EXISTS(SELECT 1 FROM approval_pair_handoffs h JOIN approvals a USING(request_id)
                 WHERE h.request_id=?1 AND h.released=1 AND h.abandoned_reason IS NULL AND a.command_block_id IS NOT NULL
                 AND a.output_block_id IS NOT NULL AND a.pair_owner IS NOT NULL)",
            [request], |row| row.get(0),
        )?)
    }

    /// Release a paired caller inside the Waiting result's journal transaction.
    pub(crate) fn release_approval_pair(&self, request: &str) -> KernelDbResult<()> {
        if !self.approval_pair_expected(request)? { return Ok(()); }
        if self.conn.is_autocommit() { return Err(KernelDbError::Validation("approval handoff requires the result transaction".into())); }
        let changed = self.conn.execute(
            "UPDATE approval_pair_handoffs SET released=1 WHERE request_id=?1 AND abandoned_reason IS NULL
             AND EXISTS(SELECT 1 FROM approvals WHERE request_id=?1 AND command_block_id IS NOT NULL
                 AND output_block_id IS NOT NULL AND pair_owner IS NOT NULL)", [request],
        )?;
        if changed != 1 { return Err(KernelDbError::Validation("approval handoff requires a complete pair link".into())); }
        Ok(())
    }

    pub(crate) fn approval_pair_abandoned_reason(&self, request: &str) -> KernelDbResult<Option<String>> {
        Ok(self.conn.query_row("SELECT abandoned_reason FROM approval_pair_handoffs WHERE request_id=?1",
            [request], |row| row.get(0)).optional()?.flatten())
    }

    /// Retire an unpublished invocation without replacing its reviewer's answer.
    /// Joins result acceptance; errors require rollback by the caller.
    pub(crate) fn abandon_approval_pair(&self, request: &str, pair: Option<(&BlockId, &BlockId)>, reason: &str) -> KernelDbResult<()> {
        if self.conn.is_autocommit() { return Err(KernelDbError::Validation("approval abandonment requires a transaction".into())); }
        let changed = self.conn.execute(
            "UPDATE approval_pair_handoffs SET abandoned_reason=?2
             WHERE request_id=?1 AND released=0 AND abandoned_reason IS NULL", rusqlite::params![request, reason])?;
        if changed == 0 { return Ok(()); }
        let row = self.get_approval(request)?.ok_or_else(|| approval_ledger::error::LedgerError::NotFound(request.into()))?;
        if let Some((command, output)) = pair {
            if row.context_id != command.context_id.as_bytes() || output.context_id != command.context_id
                || row.actor_id.as_deref() != Some(command.principal_id.as_bytes().as_slice()) {
                return Err(KernelDbError::Validation("approval abandonment does not match its context and performer".into()));
            }
            match (row.command_block_id.as_deref(), row.output_block_id.as_deref(), row.pair_owner) {
                (None, None, None) => {}
                (Some(prior_command), Some(prior_output), Some(_))
                    if prior_command == command.to_key() && prior_output == output.to_key() => {}
                _ => return Err(KernelDbError::Validation("approval abandonment belongs to a different block pair".into())),
            }
        } else if row.command_block_id.is_some() || row.output_block_id.is_some() || row.pair_owner.is_some() {
            return Err(KernelDbError::Validation("linked approval abandonment requires its original pair".into()));
        }
        match row.status {
            crate::ApprovalStatus::Pending | crate::ApprovalStatus::Claimed => {
                approval_ledger::decide::abandon(self.conn_for_ledger(), request, Some(reason))?;
            }
            crate::ApprovalStatus::Allowed | crate::ApprovalStatus::Denied => { self.redeem_ask(request)?; }
            crate::ApprovalStatus::Abandoned if row.decided_option.as_deref() == Some("cancel") => { self.redeem_ask(request)?; }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn unpublished_approval_pairs(&self) -> KernelDbResult<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT request_id FROM approval_pair_handoffs WHERE released=0 AND abandoned_reason IS NULL ORDER BY request_id")?;
        Ok(stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// One approval row, whole — the executable source, the cwd, the block
    /// pair and the decision, all of which the answer summary omits.
    ///
    /// The gate-resume driver reads this before acting on an answer:
    /// whether an ask executes on approval or its caller must retry is a
    /// property of the row, not of the summary.
    pub fn get_approval(
        &self,
        request_id: &str,
    ) -> KernelDbResult<Option<approval_ledger::types::ApprovalRow>> {
        Ok(approval_ledger::ask::get_approval(
            self.conn_for_ledger(),
            request_id,
        )?)
    }

    /// Consume an answered ask's single-use delivery. `Ok(true)` means THIS
    /// call inserted the redemption row and owns the action; `Ok(false)`
    /// means someone else already has it.
    ///
    /// The public face of `approval_ledger::decide::redeem_ask`, for the
    /// same reason the reads above are wrapped: exactly-once lives in the
    /// `approval_redemptions` primary key, and the server reaches it
    /// through the kernel rather than through the ledger connection.
    pub fn redeem_ask(&self, request_id: &str) -> KernelDbResult<bool> {
        Ok(approval_ledger::decide::redeem_ask(
            self.conn_for_ledger(),
            request_id,
        )?)
    }

    /// Whether an answer to this ask has been spent: delivered, claimed by
    /// the approval worker, or learned from.
    pub(crate) fn ask_redeemed(&self, request_id: &str) -> KernelDbResult<bool> {
        Ok(approval_ledger::ask::redeemed_at(self.conn_for_ledger(), request_id)?.is_some())
    }

    /// End a model turn's hold on its ask because the turn stopped waiting.
    /// An unanswered ask is abandoned with `reason`; an answer nobody has run
    /// is spent without running, so the approval worker never acts on it. Returns
    /// [`HeldAskEnd::Claimed`] when the worker already owns the answer: it
    /// is running or settling the command, and it settles the pair.
    pub(crate) fn end_held_ask(&self, request_id: &str, reason: &str) -> KernelDbResult<HeldAskEnd> {
        self.in_transaction(|db| {
            let conn = db.conn_for_ledger();
            let row = approval_ledger::ask::get_approval(conn, request_id)?
                .ok_or_else(|| KernelDbError::Validation(format!("held ask {request_id} has no row")))?;
            // An ordinary abandonment is terminal and never delivered, so it
            // needs no redemption.
            if matches!(row.status, crate::ApprovalStatus::Pending | crate::ApprovalStatus::Claimed) {
                approval_ledger::decide::abandon(conn, request_id, Some(reason))?;
                return Ok(HeldAskEnd::Abandoned);
            }
            Ok(if approval_ledger::decide::redeem_ask(conn, request_id)? { HeldAskEnd::Spent } else { HeldAskEnd::Claimed })
        })
    }

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

    /// Insert a new document.
    pub fn insert_document(&self, row: &DocumentRow) -> KernelDbResult<()> {
        Self::write_document(&self.conn, row)
            .map_err(|e| self.classify_document_insert_error(e, row))
    }

    /// Insert a document row against `conn` (a `&Connection` or a
    /// `&Transaction` via deref). Shared by `insert_document` and the
    /// transactional `insert_document_with_op`. Returns the raw SQLite error
    /// so the caller can classify a constraint violation by reading the
    /// database back. Does NOT commit.
    fn write_document(conn: &Connection, row: &DocumentRow) -> SqliteResult<()> {
        conn.execute(
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
        )?;
        Ok(())
    }

    /// Insert a document row and its first op in ONE transaction: both commit
    /// or neither does.
    ///
    /// A document row that commits without its first op is unreadable. The
    /// boot replay finds no snapshot and no oplog, builds an empty document,
    /// and every read of it answers "exists but has no blocks" — the shape
    /// `BlockStore::load_from_db`'s boot repair now sweeps. Writing the two
    /// rows in separate transactions is what leaves that gap;
    /// `BlockStore::create_document_with_block` closes it by coming here.
    ///
    /// `seq` is the op's oplog sequence, 1 for a document's first op.
    pub fn insert_document_with_op(
        &mut self,
        row: &DocumentRow,
        seq: i64,
        payload: &[u8],
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        let doc_err = Self::write_document(&tx, row).err();
        let op_err = match doc_err {
            None => Self::write_op(&tx, row.document_id, seq, payload).err(),
            Some(_) => None,
        };
        if doc_err.is_none() && op_err.is_none() {
            tx.commit()?;
            return Ok(());
        }
        tx.rollback()?;
        match doc_err {
            Some(e) => Err(self.classify_document_insert_error(e, row)),
            None => Err(op_err.expect("one of the two writes failed").into()),
        }
    }

    /// Classify an `insert_document` constraint-violation failure by READING
    /// THE DB BACK, never by inspecting the SQLite/kaijutsu error message
    /// text — `map_unique_violation` flattens every UNIQUE conflict into the
    /// same `LabelConflict` string, which makes a benign PRIMARY KEY
    /// duplicate indistinguishable from other constraint violations by
    /// message text alone.
    ///
    /// FK violations (extended code 787) and anything that isn't a
    /// constraint violation delegate straight to `map_unique_violation`,
    /// preserving today's `Validation` mapping exactly.
    fn classify_document_insert_error(
        &self,
        e: rusqlite::Error,
        row: &DocumentRow,
    ) -> KernelDbError {
        let is_constraint_violation = matches!(
            &e,
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation
                    && err.extended_code != 787
        );
        if !is_constraint_violation {
            return map_unique_violation(e, "document already exists or path conflict");
        }

        // Which constraint fired? Read back, don't guess from the message.
        match self.get_document(row.document_id) {
            Ok(Some(_)) => return KernelDbError::DuplicateDocument(row.document_id),
            Ok(None) => {}
            Err(read_err) => return read_err,
        }
        // The read-back doesn't explain the failure — don't fabricate a
        // classification, fall through to the existing generic mapping.
        map_unique_violation(e, "document already exists or path conflict")
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
        let rows = stmt.query_map([], row_to_document_row)?;
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

    /// Delete a document that has no context. Context history is retained.
    pub fn delete_document(&self, id: ContextId) -> KernelDbResult<bool> {
        let tx = self.conn.unchecked_transaction()?;
        if self.get_context(id)?.is_some() {
            return Err(KernelDbError::Validation(format!(
                "context history is retained; use `kj context archive {id}`"
            )));
        }
        let deleted = tx.execute(
            "DELETE FROM documents WHERE document_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        tx.commit()?;
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
        Self::write_op(&self.conn, document_id, seq, payload)?;
        Ok(())
    }

    /// Append an op row against `conn` (a `&Connection` or a `&Transaction`
    /// via deref). Shared by `append_op` and the transactional
    /// `insert_document_with_op`. Does NOT commit.
    fn write_op(
        conn: &Connection,
        document_id: ContextId,
        seq: i64,
        payload: &[u8],
    ) -> SqliteResult<()> {
        conn.execute(
            "INSERT INTO oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
            params![blob_param(document_id.as_bytes()), seq, payload],
        )?;
        Ok(())
    }

    /// Store the byte-exact original an ingest transform consumed for this
    /// block (docs/ansi-and-beyond.md). `OR REPLACE`: originals are
    /// write-once in practice, but re-running an ingest hook on the same
    /// block must not fail — last write wins, and both writes carry the same
    /// bytes by construction (the hook writes what it just stripped).
    pub fn insert_block_provenance(
        &self,
        block_id: &BlockId,
        transform: &str,
        version: u32,
        original: &[u8],
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO block_provenance
                 (context_id, principal_id, block_seq, transform, version, original)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                blob_param(block_id.context_id.as_bytes()),
                blob_param(block_id.principal_id.as_bytes()),
                block_id.seq as i64,
                transform,
                version,
                original,
            ],
        )?;
        Ok(())
    }

    /// Fetch the stored original for one (block, transform), or `None` — a
    /// block whose `provenance` tag names a transform with no row here is a
    /// detectable gap (crash between the block write and this row), and
    /// callers report it rather than inventing bytes.
    pub fn get_block_provenance(
        &self,
        block_id: &BlockId,
        transform: &str,
    ) -> KernelDbResult<Option<(u32, Vec<u8>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT version, original FROM block_provenance
                 WHERE context_id = ?1 AND principal_id = ?2
                   AND block_seq = ?3 AND transform = ?4",
                params![
                    blob_param(block_id.context_id.as_bytes()),
                    blob_param(block_id.principal_id.as_bytes()),
                    block_id.seq as i64,
                    transform,
                ],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// List the transforms holding an original for this block:
    /// `(transform, version, original_len)`. The bytes stay in the store —
    /// this is the cheap "what could reproject?" question.
    pub fn list_block_provenance(
        &self,
        block_id: &BlockId,
    ) -> KernelDbResult<Vec<(String, u32, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT transform, version, length(original) FROM block_provenance
             WHERE context_id = ?1 AND principal_id = ?2 AND block_seq = ?3
             ORDER BY transform",
        )?;
        let rows = stmt
            .query_map(
                params![
                    blob_param(block_id.context_id.as_bytes()),
                    blob_param(block_id.principal_id.as_bytes()),
                    block_id.seq as i64,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, i64>(2)? as usize,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
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
            "SELECT document_id, seq, version, state, created_at
             FROM doc_snapshots WHERE document_id = ?1",
        )?;
        let mut rows = stmt.query(params![blob_param(document_id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(DocSnapshotRow {
                document_id: read_context_id(row, 0)?,
                seq: row.get(1)?,
                version: row.get(2)?,
                state: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Write a compaction snapshot and truncate the oplog up to that seq.
    /// Joins an existing transaction, or commits both writes in its own.
    pub fn write_snapshot_and_truncate(
        &self,
        document_id: ContextId,
        seq: i64,
        version: i64,
        state: &[u8],
    ) -> KernelDbResult<()> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.write_snapshot_and_truncate(document_id, seq, version, state));
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO doc_snapshots (document_id, seq, version, state)
             VALUES (?1, ?2, ?3, ?4)",
            params![blob_param(document_id.as_bytes()), seq, version, state],
        )?;
        self.conn.execute(
            "DELETE FROM oplog WHERE document_id = ?1 AND seq <= ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
        Ok(())
    }

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
        if row.played_by.is_some() {
            return Err(KernelDbError::Validation(format!(
                "context {} sets played_by, which is read from its context character; write the context character instead",
                row.context_id.short()
            )));
        }

        conn.execute(
                "INSERT INTO contexts (
                context_id, label,
                system_prompt, context_state, context_type,
                created_at, created_by, forked_from, fork_kind,
                archived_at, workspace_id, preset_id, concluded_at,
                last_activity_at, promoted_at, demoted_at, paused_at,
                origin_host
            ) VALUES (
                ?1, ?2,
                ?3, ?4, ?5,
                ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17,
                ?18
            )",
                params![
                    blob_param(row.context_id.as_bytes()),
                    row.label,
                    row.system_prompt,
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
                    row.last_activity_at,
                    row.promoted_at,
                    row.demoted_at,
                    row.paused_at,
                    row.origin_host,
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
            concat!("SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, ", performer_sql!("contexts"), " AS played_by
             FROM contexts WHERE context_id = ?1"),
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

    /// Update a context's settings.
    pub fn update_settings(
        &self,
        id: ContextId,
        system_prompt: Option<&str>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET system_prompt = ?1
             WHERE context_id = ?2",
            params![system_prompt, blob_param(id.as_bytes())],
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

    /// Set (or clear) a context's advisory `origin_host` — the hostname the
    /// registering client self-reported. Deliberately NOT part of
    /// `write_context`'s own INSERT: it is set once, right after creation,
    /// by the `setContextOriginHost` RPC handler, so creation paths with no
    /// client to ask (genesis bootstrap, fork, tests) simply never call this
    /// and the row keeps `origin_host = NULL` ("-" on display). `host =
    /// None` clears an existing value. Unknown context id is a loud
    /// `NotFound`, same convention as `update_label`/`update_context_type`.
    pub fn set_origin_host(&self, id: ContextId, host: Option<&str>) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE contexts SET origin_host = ?1 WHERE context_id = ?2",
            params![host, blob_param(id.as_bytes())],
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

    /// Archive a context while retaining its history. Returns true if it was
    /// active. Clears ring placement and abandons unresolved asks without
    /// deleting their records. Decided asks retain their answers. State and
    /// sweep commit together; an already archived row retries unresolved ask
    /// cleanup. This method does not stop running work.
    pub fn archive_context(&self, id: ContextId) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.archive_context_writes(id));
        }
        self.conn.execute_batch("SAVEPOINT archive_context")?;
        let result = self.archive_context_writes(id);
        match result {
            Ok(archived) => {
                self.conn.execute_batch("RELEASE SAVEPOINT archive_context")?;
                Ok(archived)
            }
            Err(error) => {
                self.conn.execute_batch(
                    "ROLLBACK TO SAVEPOINT archive_context; RELEASE SAVEPOINT archive_context",
                )?;
                Err(error)
            }
        }
    }

    /// Why `id` may not be archived yet: it holds drifts whose asks are
    /// unanswered or unsettled. A held drift is settled by its ask, and an
    /// archived context answers nothing, so archiving would leave the drift
    /// held with no ask that can release it. Verbs that archive check this
    /// before they run anything; [`Self::archive_context_writes`] checks it
    /// again inside the write.
    pub(crate) fn held_drift_refusal(&self, id: ContextId) -> KernelDbResult<Option<String>> {
        let held = approval_ledger::ask::list_unsettled_drift_asks(&self.conn, id.as_bytes())?;
        if held.is_empty() {
            return Ok(None);
        }
        let asks = held.iter().map(|id| id.split('-').next().unwrap_or(id)).collect::<Vec<_>>().join(", ");
        Ok(Some(format!(
            "context {} holds {} drift(s) waiting for its human (ask {asks}); answer each with \
             `kj ledger allow <id>` or `kj ledger deny <id>`, then run the verb again",
            id.short(), held.len()
        )))
    }

    fn archive_context_writes(&self, id: ContextId) -> KernelDbResult<bool> {
        if let Some(why) = self.held_drift_refusal(id)? {
            return Err(KernelDbError::Validation(why));
        }
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts
                SET archived_at = ?1, context_state = 'archived',
                    promoted_at = NULL, demoted_at = NULL
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        let swept = approval_ledger::decide::abandon_unresolved_for_archived_context(
            &self.conn,
            id.as_bytes(),
            "the context that raised this was archived; this ask cannot authorize further work",
        )?;
        if swept > 0 {
            info!(context = %id.short(), swept, "archived context: swept unresolved asks");
        }
        Ok(updated > 0)
    }

    /// Finish a rotation in one transaction: archive `predecessor`, give
    /// `successor` its label and ring placement, and point a character's
    /// `handoff_ctx` at the successor when it named the predecessor. Returns
    /// how many handoff pointers moved. A predecessor that is missing or
    /// already archived refuses. Moves a character's `root_ctx` the same way.
    /// The kernel moves no kohai.
    pub fn commit_rotation(&self, predecessor: ContextId, successor: ContextId) -> KernelDbResult<usize> {
        self.in_transaction(|db| {
            let row = db.get_context(predecessor)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("context {}", predecessor.short()))
            })?;
            if !db.archive_context(predecessor)? {
                return Err(KernelDbError::Validation(format!(
                    "context {} was archived before the rotation finished",
                    predecessor.short()
                )));
            }
            db.conn.execute(
                "UPDATE contexts SET label = ?1, promoted_at = ?2, demoted_at = ?3 WHERE context_id = ?4",
                params![row.label, row.promoted_at, row.demoted_at, blob_param(successor.as_bytes())],
            )?;
            let moved = db.conn.execute(
                "UPDATE characters SET handoff_ctx = ?1 WHERE handoff_ctx = ?2",
                params![blob_param(successor.as_bytes()), blob_param(predecessor.as_bytes())],
            )?;
            db.conn.execute(
                "UPDATE characters SET root_ctx = ?1 WHERE root_ctx = ?2",
                params![blob_param(successor.as_bytes()), blob_param(predecessor.as_bytes())],
            )?;
            Ok(moved)
        })
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
    /// context was unknown, archived, or already concluded. Also clears
    /// `promoted_at` — concluding is the mux-exit that gives up the ring-0
    /// seat — but deliberately preserves `demoted_at`: explicit demotion is
    /// sticky through conclusion, so a demoted context stays in the demoted
    /// ring instead of floating back to the auto pool.
    pub fn conclude_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts
                SET context_state = 'concluded', concluded_at = ?1, promoted_at = NULL
             WHERE context_id = ?2
               AND archived_at IS NULL
               AND concluded_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Promote a context into ring 0 ("active"). First-write-wins on
    /// `promoted_at` — re-promoting an already-promoted context is a no-op
    /// that preserves the original timestamp, so ring-0 order stays
    /// append-stable — and clears `demoted_at`. Promoting an ARCHIVED
    /// context resurrects it: `archived_at` cleared, a fresh `promoted_at`
    /// stamped (the old seat was surrendered at archive time), `concluded_at`
    /// untouched — see [`PromoteOutcome::Resurrected`]. Enforces
    /// [`ACTIVE_RING_CAPACITY`] on every seat grant (resurrection included):
    /// a full ring fails loudly rather than silently bumping anything —
    /// ring-0 is a hand-curated row of seats addressed by digit hotkeys, so
    /// seats must never appear or vanish without an explicit act. On the
    /// ring-full error an archived context STAYS archived (no partial write).
    pub fn promote_context(&self, id: ContextId) -> KernelDbResult<PromoteOutcome> {
        let (promoted_at, archived_at, concluded_at): (Option<i64>, Option<i64>, Option<i64>) =
            self.conn
                .query_row(
                    "SELECT promoted_at, archived_at, concluded_at
                     FROM contexts WHERE context_id = ?1",
                    params![blob_param(id.as_bytes())],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| KernelDbError::NotFound(format!("context {}", id.short())))?;

        if archived_at.is_none() && promoted_at.is_some() {
            return Ok(PromoteOutcome::AlreadyPromoted);
        }

        let active_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts WHERE promoted_at IS NOT NULL AND archived_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        if active_count >= ACTIVE_RING_CAPACITY {
            return Err(KernelDbError::Validation(format!(
                "active ring full ({ACTIVE_RING_CAPACITY} seats) — demote something first"
            )));
        }

        if archived_at.is_some() {
            // Resurrection: unarchive + a FRESH promoted_at in one write.
            // `archive_context` clears promoted_at, so an archived row
            // shouldn't still carry one — but stamp unconditionally anyway:
            // the old seat was surrendered at archive time either way.
            self.conn.execute(
                // The restored state is derived from `concluded_at`, not
                // remembered from before the archive: the timestamps are what
                // survive, so they are what the state is read back out of.
                "UPDATE contexts
                    SET archived_at = NULL,
                        context_state = CASE WHEN concluded_at IS NULL
                                             THEN 'live' ELSE 'concluded' END,
                        promoted_at = ?1, demoted_at = NULL
                 WHERE context_id = ?2",
                params![now_millis(), blob_param(id.as_bytes())],
            )?;
            return Ok(PromoteOutcome::Resurrected {
                concluded: concluded_at.is_some(),
            });
        }

        self.conn.execute(
            "UPDATE contexts SET promoted_at = ?1, demoted_at = NULL
             WHERE context_id = ?2 AND promoted_at IS NULL",
            params![now_millis(), blob_param(id.as_bytes())],
        )?;
        Ok(PromoteOutcome::Promoted)
    }

    /// Push a context outward one step on the demote ladder (kernel-owned
    /// policy, one place): already demoted → archives it (reuses
    /// [`Self::archive_context`]'s write — single context, no subtree
    /// recursion, no latch); promoted → clears `promoted_at` (back to
    /// automatic placement); neither → sets `demoted_at`. Each call is
    /// exactly one step outward. Errors if the context is unknown or already
    /// archived (nothing further out to step to).
    ///
    /// `demoted_at` is checked FIRST. On every legal state the order is
    /// irrelevant (the kernel keeps the two stamps mutually exclusive), but
    /// on a corrupt both-stamps row it makes this ladder agree with the app
    /// layout's demoted-wins rule (`kaijutsu_viz::layout::assign_ring_seats`)
    /// instead of no-op-looping through unpromote while the card sits in
    /// ring 3.
    pub fn demote_context(&self, id: ContextId) -> KernelDbResult<DemoteOutcome> {
        let (promoted_at, demoted_at, archived_at): (Option<i64>, Option<i64>, Option<i64>) = self
            .conn
            .query_row(
                "SELECT promoted_at, demoted_at, archived_at FROM contexts WHERE context_id = ?1",
                params![blob_param(id.as_bytes())],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", id.short())))?;

        if archived_at.is_some() {
            return Err(KernelDbError::Validation(
                "cannot demote an archived context".to_string(),
            ));
        }

        if demoted_at.is_some() {
            self.archive_context(id)?;
            Ok(DemoteOutcome::Archived)
        } else if promoted_at.is_some() {
            self.conn.execute(
                "UPDATE contexts SET promoted_at = NULL WHERE context_id = ?1",
                params![blob_param(id.as_bytes())],
            )?;
            Ok(DemoteOutcome::Unpromoted)
        } else {
            self.conn.execute(
                "UPDATE contexts SET demoted_at = ?1 WHERE context_id = ?2",
                params![now_millis(), blob_param(id.as_bytes())],
            )?;
            Ok(DemoteOutcome::Demoted)
        }
    }

    /// Set or clear a context's `paused_at`. Design-only for now — see the
    /// doc on [`ContextRow::paused_at`] for the intended (not yet wired)
    /// gating semantics. Unlike `promoted_at`'s first-write-wins, this is a
    /// plain on/off flag: it unconditionally overwrites on every call.
    /// Refuses archived contexts loudly (symmetric with demote's guard);
    /// unknown ids stay `NotFound`.
    pub fn set_context_paused(&self, id: ContextId, paused: bool) -> KernelDbResult<()> {
        let value = if paused { Some(now_millis()) } else { None };
        let updated = self.conn.execute(
            "UPDATE contexts SET paused_at = ?1
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![value, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            // Distinguish "no such row" from "row exists but is archived" —
            // conflating them would misreport a real context as unknown.
            let archived: Option<bool> = self
                .conn
                .query_row(
                    "SELECT archived_at IS NOT NULL FROM contexts WHERE context_id = ?1",
                    params![blob_param(id.as_bytes())],
                    |row| row.get(0),
                )
                .optional()?;
            let verb = if paused { "pause" } else { "resume" };
            return Err(match archived {
                Some(true) => {
                    KernelDbError::Validation(format!("cannot {verb} an archived context"))
                }
                _ => KernelDbError::NotFound(format!("context {}", id.short())),
            });
        }
        Ok(())
    }

    /// Stamp `last_activity_at` on a context — called from
    /// `BlockStore::journal_op`, the one chokepoint every mutating block op
    /// (insert, status change, text edit...) funnels through. Unlike
    /// `concluded_at` (first-write-wins), this re-stamps on every call: it is
    /// the kernel-truth "recency" fact the time-well vortex orders by.
    ///
    /// `ts` must be the same clock/epoch as `created_at` (Unix millis via
    /// `now_millis()` / `kaijutsu_types::now_millis()`) — callers compute
    /// `now − last_activity_at` directly against it, so a unit mismatch
    /// corrupts idle-age math.
    pub fn touch_context_activity(&self, context_id: ContextId, ts: i64) -> KernelDbResult<()> {
        self.conn.execute(
            "UPDATE contexts SET last_activity_at = ?2 WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes()), ts],
        )?;
        Ok(())
    }

    /// List active (non-archived) contexts.
    pub fn list_active_contexts(&self) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            concat!("SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, ", performer_sql!("contexts"), " AS played_by
             FROM contexts
             WHERE archived_at IS NULL
             ORDER BY COALESCE(last_activity_at, created_at)"),
        )?;

        let rows = stmt.query_map([], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// List all contexts (including archived).
    pub fn list_all_contexts(&self) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            concat!("SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, ", performer_sql!("contexts"), " AS played_by
             FROM contexts
             ORDER BY COALESCE(last_activity_at, created_at)"),
        )?;

        let rows = stmt.query_map([], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Resolve a context query string.
    ///
    /// Supports exact label, label prefix, hex prefix. For tag:prefix syntax
    /// (future), walks lineage via CTE.
    ///
    /// Fuzzy forms (label, prefix) resolve over the ACTIVE set only — an
    /// archived context never shadows a live label or shows up in prefix
    /// ambiguity. A FULL id (32-char hex or hyphenated UUID) resolves
    /// against the whole table, archived included: the exact id is the
    /// resurrection handle (`kj context promote <full-id>` must be able to
    /// reach an archived context; promote is the resurrection door).
    pub fn resolve_context(&self, query: &str) -> KernelDbResult<ContextId> {
        if let Ok(full) = ContextId::parse(query)
            && self.get_context(full)?.is_some()
        {
            return Ok(full);
        }

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

    /// Get the live children of a context: the contexts forked from it.
    pub fn fork_children(&self, id: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            concat!("SELECT c.context_id, c.label,
                    c.system_prompt, c.context_state, c.context_type,
                    c.created_at, c.created_by, c.forked_from, c.fork_kind,
                    c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                    c.last_activity_at, c.promoted_at, c.demoted_at, c.paused_at, c.origin_host, ", performer_sql!("c"), " AS played_by
             FROM contexts c
             WHERE c.forked_from = ?1 AND c.archived_at IS NULL
             ORDER BY c.created_at, c.context_id"),
        )?;

        let rows = stmt.query_map(params![blob_param(id.as_bytes())], |row| {
            row_to_context_row(row)
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Every live context in depth-first order over `forked_from`, each
    /// parent followed by its subtree, siblings by creation time. Returns
    /// `(ContextRow, depth)` pairs. A live context whose parent is archived
    /// (or absent) is a root of its own, so it still appears.
    pub fn context_dag(&self) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let live = self.list_active_contexts()?;
        let live_ids: HashSet<ContextId> = live.iter().map(|c| c.context_id).collect();
        let mut children: HashMap<ContextId, Vec<&ContextRow>> = HashMap::new();
        let mut roots: Vec<&ContextRow> = Vec::new();
        for ctx in &live {
            match ctx.forked_from.filter(|p| live_ids.contains(p)) {
                Some(parent) => children.entry(parent).or_default().push(ctx),
                None => roots.push(ctx),
            }
        }
        let order = |a: &&ContextRow, b: &&ContextRow| {
            (a.created_at, a.context_id).cmp(&(b.created_at, b.context_id))
        };
        roots.sort_by(order);
        for siblings in children.values_mut() {
            siblings.sort_by(order);
        }

        let mut out = Vec::with_capacity(live.len());
        let mut stack: Vec<(&ContextRow, i64)> = roots.into_iter().rev().map(|c| (c, 0)).collect();
        while let Some((ctx, depth)) = stack.pop() {
            out.push((ctx.clone(), depth));
            if let Some(kids) = children.get(&ctx.context_id) {
                stack.extend(kids.iter().rev().map(|c| (*c, depth + 1)));
            }
        }
        if out.len() != live.len() {
            return Err(KernelDbError::Validation(format!(
                "forked_from forms a cycle among live contexts: {} of {} reachable from a root",
                out.len(),
                live.len()
            )));
        }
        Ok(out)
    }

    /// Walk fork lineage from a context upward via `forked_from`.
    /// Returns `(ContextRow, depth)` where depth 0 = the starting context.
    pub fn fork_lineage(&self, context_id: ContextId) -> KernelDbResult<Vec<(ContextRow, i64)>> {
        let mut stmt = self.conn.prepare(
            concat!("WITH RECURSIVE lineage(ctx_id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT c.forked_from, lineage.depth + 1
                FROM lineage
                JOIN contexts c ON c.context_id = lineage.ctx_id
                WHERE c.forked_from IS NOT NULL
            )
            SELECT c.context_id, c.label,
                   c.system_prompt, c.context_state, c.context_type,
                   c.created_at, c.created_by, c.forked_from, c.fork_kind,
                   c.archived_at, c.workspace_id, c.preset_id, c.concluded_at,
                   c.last_activity_at, c.promoted_at, c.demoted_at, c.paused_at,
                   c.origin_host, ", performer_sql!("c"), " AS played_by, lineage.depth
            FROM lineage
            JOIN contexts c ON c.context_id = lineage.ctx_id
            ORDER BY lineage.depth"),
        )?;

        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            let ctx = row_to_context_row(row)?;
            let depth: i64 = row.get(19)?;
            Ok((ctx, depth))
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ========================================================================
    // Context Edges
    // ========================================================================

    /// Insert a drift edge.
    pub fn insert_edge(&self, row: &ContextEdgeRow) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_edges (edge_id, source_id, target_id, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.edge_id.as_bytes().as_slice(),
                blob_param(row.source_id.as_bytes()),
                blob_param(row.target_id.as_bytes()),
                row.metadata,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    /// List edges from a source, oldest first.
    pub fn edges_from(&self, source: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, metadata, created_at
             FROM context_edges
             WHERE source_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![blob_param(source.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// List edges to a target, oldest first.
    pub fn edges_to(&self, target: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, metadata, created_at
             FROM context_edges
             WHERE target_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![blob_param(target.as_bytes())], row_to_edge_row)?
            .collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// List drift edges originating from a context, newest first.
    pub fn drift_provenance(&self, context_id: ContextId) -> KernelDbResult<Vec<ContextEdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT edge_id, source_id, target_id, metadata, created_at
             FROM context_edges
             WHERE source_id = ?1
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

    /// Delete one oplog row, simulating the two states `block_content_at_seq`
    /// must refuse to paper over: a journal write that has claimed its seq but
    /// not yet committed (the TOCTOU window in `journal_op`), and a row lost
    /// to a failed `append_op`. There is no public API that produces a gapped
    /// oplog on purpose — that's the point. Never compiled outside test builds.
    #[cfg(test)]
    pub(crate) fn delete_oplog_row_for_test(
        &self,
        document_id: ContextId,
        seq: i64,
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "DELETE FROM oplog WHERE document_id = ?1 AND seq = ?2",
            params![blob_param(document_id.as_bytes()), seq],
        )?;
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

    /// Delete a single drift edge by its UUID.
    ///
    /// Companion to `kj drift edge rm` — `kj drift history` emits these
    /// UUIDs as iteration handles. A context's parent link is
    /// `contexts.forked_from`, so no edge row holds one.
    pub fn delete_drift_edge(&self, edge_id: uuid::Uuid) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM context_edges
             WHERE edge_id = ?1",
            params![edge_id.as_bytes().as_slice()],
        )?;
        Ok(deleted > 0)
    }

    /// Find the live context that currently holds a given label. Archived
    /// contexts keep their label but never hold it: `None` when only
    /// archived rows carry it, so the label is free to give again.
    pub fn find_context_by_label(&self, label: &str) -> KernelDbResult<Option<ContextRow>> {
        let mut stmt = self.conn.prepare(
            concat!("SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at, origin_host, ", performer_sql!("contexts"), " AS played_by
             FROM contexts WHERE label = ?1 AND archived_at IS NULL"),
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_context_row(row)?))
        } else {
            Ok(None)
        }
    }

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

/// One configured LLM endpoint. `name` is the free-form handle; `kind` is the
/// closed code-owned enum. See the `backends` table comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRow {
    pub backend_id: BackendId,
    pub name: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub key_optional: bool,
    pub request_timeout_secs: Option<i64>,
    pub idle_timeout_secs: Option<i64>,
    pub max_concurrent: Option<i64>,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// Per-model metadata for one backend (a `backend_models` row).
#[derive(Debug, Clone, PartialEq)]
pub struct BackendModelRow {
    pub backend_id: BackendId,
    pub model_id: String,
    /// Total context window (input + output) in tokens. `None` = unknown,
    /// never a guess.
    pub context_window: Option<i64>,
    /// Sparse provider-specific JSON.
    pub extra: Option<String>,
    /// This model's own tunables on this backend.
    pub tunables: ModelTunablesRow,
}

/// A model's tunables on its backend. A binding copies each into a slot whose
/// own value is `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelTunablesRow {
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

/// A character's sheet — the persistent someone `name` resolves to. The
/// principal is the character's key (`docs/character.md`, "Character =
/// principal + sheet"): no distinct id type, `principal_id` is immutable,
/// and `name` is kernel-owned and immutable in this slice. `handoff_ctx`
/// arrives with slice 4; the rest of the sheet arrives with the slice that
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterRow {
    pub principal_id: PrincipalId,
    pub name: String,
    pub created_at: i64,
    /// `None` while live. Set once, never cleared — a character retires,
    /// it is never un-retired.
    pub retired_at: Option<i64>,
    /// The character's handoff-log context, or `None` before the first `kj
    /// handoff note`/`tail` mints one. Set exactly once, by
    /// `set_character_handoff_ctx` under the same `KernelDb` lock that
    /// checked it was still unset (`docs/character.md`, "The handoff is an
    /// ordinary context").
    pub handoff_ctx: Option<ContextId>,
    /// A root character's root context, where its person types `kj` and
    /// where the seats it starts begin their lineage. `None` for an
    /// ordinary character, and briefly for a root whose context the kernel
    /// has not created yet (`docs/character.md`, "Roots and rotation").
    pub root_ctx: Option<ContextId>,
    /// A root character is a model context: a model can play it, and its
    /// kj requests always need approval (`docs/character.md`, "Roots and
    /// rotation").
    pub root: bool,
    /// The context type this character usually plays, which `kj context
    /// create <character>` takes when `--type` is absent. `None` for no
    /// type, and always for a root.
    pub context_type: Option<String>,
}

impl CharacterRow {
    /// A root character that has not retired: a person's hands on the
    /// instrument, the posture every "is this a person" check asks about.
    pub fn is_live_root(&self) -> bool {
        self.root && self.retired_at.is_none()
    }
}

/// One role's seat on a character. A NULL tunable takes the model's `backend_models` value when a context character copies the slot.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterSlotRow {
    pub principal_id: PrincipalId,
    pub role: String,
    pub backend_id: BackendId,
    pub model: String,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

/// A character's one model. A NULL tunable takes the model's `backend_models` value when a context character copies it.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterModelRow {
    pub principal_id: PrincipalId,
    pub backend_id: BackendId,
    pub model: String,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub effort: Option<String>,
    pub thinking_budget: Option<i64>,
    pub thinking_style: Option<String>,
}

/// A `--model` shorthand: alias → (backend, model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAliasRow {
    pub alias: String,
    pub backend_id: BackendId,
    pub model: String,
}

/// The retired risk classifier's global pre_call hook: its id and the body
/// path its rc installed. Opening a DB removes a row matching both.
pub(crate) const RETIRED_CLASSIFIER_HOOK: (&str, &str) = ("lfm2d-advisory", "/config/rc/lib/hooks/lfm2d.kai");

/// Per-context env keys the retired classifier's rc exported. Opening a DB
/// removes them; nothing reads them.
pub(crate) const RETIRED_CLASSIFIER_ENV: &[&str] = &["LFM2D_MODE", "LFM2D_BENIGN_LABEL", "LFM2D_URL"];

/// The singleton `embedding_config` row. `backend` names a `backends` row of
/// kind `openai`, which owns the base URL and the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfigRow {
    pub enabled: bool,
    pub backend: String,
    pub model: String,
    pub dimensions: i64,
    /// An [`kaijutsu_index::EmbeddingProfile`] token.
    pub profile: String,
    pub timeout_ms: i64,
    pub max_in_flight: i64,
    pub max_context_bytes: i64,
}

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


const BACKEND_COLS: &str = "backend_id, name, kind, base_url, api_key_env, api_key_file, \
                            key_optional, request_timeout_secs, idle_timeout_secs, created_at, \
                            created_by, max_concurrent";

fn row_to_backend(row: &rusqlite::Row<'_>) -> SqliteResult<BackendRow> {
    Ok(BackendRow {
        backend_id: read_backend_id(row, 0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        base_url: row.get(3)?,
        api_key_env: row.get(4)?,
        api_key_file: row.get(5)?,
        key_optional: row.get::<_, i64>(6)? != 0,
        request_timeout_secs: row.get(7)?,
        idle_timeout_secs: row.get(8)?,
        created_at: row.get(9)?,
        created_by: read_principal_id(row, 10)?,
        max_concurrent: row.get(11)?,
    })
}

const CHARACTER_SLOT_COLS: &str = "principal_id, role, backend_id, model, max_tokens, temperature, \
                                   top_p, effort, thinking_budget, thinking_style";

fn row_to_character_slot(row: &rusqlite::Row<'_>) -> SqliteResult<CharacterSlotRow> {
    Ok(CharacterSlotRow {
        principal_id: read_principal_id(row, 0)?,
        role: row.get(1)?,
        backend_id: read_backend_id(row, 2)?,
        model: row.get(3)?,
        max_tokens: row.get(4)?,
        temperature: row.get(5)?,
        top_p: row.get(6)?,
        effort: row.get(7)?,
        thinking_budget: row.get(8)?,
        thinking_style: row.get(9)?,
    })
}

const CHARACTER_MODEL_COLS: &str = "principal_id, backend_id, model, max_tokens, temperature, \
                                    top_p, effort, thinking_budget, thinking_style";

fn row_to_character_model(row: &rusqlite::Row<'_>) -> SqliteResult<CharacterModelRow> {
    Ok(CharacterModelRow {
        principal_id: read_principal_id(row, 0)?,
        backend_id: read_backend_id(row, 1)?,
        model: row.get(2)?,
        max_tokens: row.get(3)?,
        temperature: row.get(4)?,
        top_p: row.get(5)?,
        effort: row.get(6)?,
        thinking_budget: row.get(7)?,
        thinking_style: row.get(8)?,
    })
}

impl KernelDb {
    // ── backends ────────────────────────────────────────────────────────

    /// Insert or update a backend, keyed on its unique `name`. Returns the
    /// row as stored (with the id that was kept or minted).
    ///
    /// Upsert semantics: an existing row with the same name keeps its
    /// `backend_id` — so cast slots and aliases pointing at it survive a
    /// re-`set` — and every other column is replaced by what the caller
    /// passed. There is no partial merge: `kj backend set` is a declaration
    /// of the whole row, and a half-applied config is worse than a loud
    /// re-type.
    pub fn upsert_backend(&self, row: &BackendRow) -> KernelDbResult<BackendRow> {
        validate_label(&row.name)?;
        // `kind` is a closed set and this is the one write path, so the set is
        // enforced here. `BackendRow.kind` is a `String` because the column is,
        // and a kind the code cannot dispatch on must fail at the write that
        // introduces it rather than at the next registry build — by then the
        // row is durable and the caller who typo'd it is gone.
        crate::llm::config::BackendKind::parse(&row.kind).map_err(KernelDbError::Validation)?;
        let existing = self.get_backend_by_name(&row.name)?;
        let backend_id = existing.as_ref().map(|b| b.backend_id).unwrap_or(row.backend_id);
        let created_at = existing.as_ref().map(|b| b.created_at).unwrap_or(row.created_at);
        self.conn.execute(
            "INSERT INTO backends (backend_id, name, kind, base_url, api_key_env, api_key_file,
                                   key_optional, request_timeout_secs, idle_timeout_secs,
                                   created_at, created_by, max_concurrent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(backend_id) DO UPDATE SET
                 name = excluded.name,
                 kind = excluded.kind,
                 base_url = excluded.base_url,
                 api_key_env = excluded.api_key_env,
                 api_key_file = excluded.api_key_file,
                 key_optional = excluded.key_optional,
                 request_timeout_secs = excluded.request_timeout_secs,
                 idle_timeout_secs = excluded.idle_timeout_secs,
                 max_concurrent = excluded.max_concurrent",
            params![
                blob_param(backend_id.as_bytes()),
                row.name,
                row.kind,
                row.base_url,
                row.api_key_env,
                row.api_key_file,
                row.key_optional as i64,
                row.request_timeout_secs,
                row.idle_timeout_secs,
                created_at,
                blob_param(row.created_by.as_bytes()),
                row.max_concurrent,
            ],
        )?;
        Ok(BackendRow {
            backend_id,
            created_at,
            ..row.clone()
        })
    }

    /// Fetch a backend by its unique name.
    pub fn get_backend_by_name(&self, name: &str) -> KernelDbResult<Option<BackendRow>> {
        let sql = format!("SELECT {BACKEND_COLS} FROM backends WHERE name = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_row(params![name], row_to_backend).optional()?)
    }

    /// Every backend, ordered by name.
    pub fn list_backends(&self) -> KernelDbResult<Vec<BackendRow>> {
        let sql = format!("SELECT {BACKEND_COLS} FROM backends ORDER BY name");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_backend)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Delete a backend by name.
    ///
    /// Refuses loudly while a character slot, a context's slot or a
    /// model alias still points at it. The FK would reject the DELETE anyway,
    /// but a named list of the referents is the difference between "fix this"
    /// and "SQLITE_CONSTRAINT".
    pub fn delete_backend(&self, name: &str) -> KernelDbResult<()> {
        let Some(backend) = self.get_backend_by_name(name)? else {
            return Err(KernelDbError::NotFound(format!("backend '{name}'")));
        };
        let id = blob_param(backend.backend_id.as_bytes());
        let slots: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.name || '/' || s.role FROM character_slots s
                 JOIN characters c ON c.principal_id = s.principal_id
                 WHERE s.backend_id = ?1 ORDER BY 1",
            )?;
            stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        let models: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT c.name FROM character_models m
                 JOIN characters c ON c.principal_id = m.principal_id
                 WHERE m.backend_id = ?1 ORDER BY 1",
            )?;
            stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        let bindings: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT cc.context_id FROM context_slots cs
                 JOIN context_characters cc ON cc.context_id = cs.context_id
                 WHERE cs.backend_id = ?1 ORDER BY 1",
            )?;
            stmt.query_map(params![id], |r| Ok(read_context_id(r, 0)?.short()))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        let aliases: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT alias FROM model_aliases WHERE backend_id = ?1 ORDER BY alias")?;
            stmt.query_map(params![id], |r| r.get::<_, String>(0))?
                .collect::<SqliteResult<Vec<_>>>()?
        };
        if !slots.is_empty() || !models.is_empty() || !aliases.is_empty() || !bindings.is_empty() {
            let mut parts = Vec::new();
            if !slots.is_empty() {
                parts.push(format!("character slots [{}]", slots.join(", ")));
            }
            if !models.is_empty() {
                parts.push(format!("the models of characters [{}]", models.join(", ")));
            }
            if !bindings.is_empty() {
                parts.push(format!("context slots of [{}]", bindings.join(", ")));
            }
            if !aliases.is_empty() {
                parts.push(format!("aliases [{}]", aliases.join(", ")));
            }
            return Err(KernelDbError::Validation(format!(
                "backend '{name}' is still referenced by {} — remove those first",
                parts.join(" and ")
            )));
        }
        self.conn
            .execute("DELETE FROM backends WHERE backend_id = ?1", params![id])?;
        Ok(())
    }

    // ── backend_models ──────────────────────────────────────────────────

    /// Insert or replace one backend's per-model metadata row.
    pub fn set_backend_model(&self, row: &BackendModelRow) -> KernelDbResult<()> {
        if row.model_id.trim().is_empty() {
            return Err(KernelDbError::Validation(
                "model id must not be empty".to_string(),
            ));
        }
        if let Some(w) = row.context_window
            && w <= 0
        {
            return Err(KernelDbError::Validation(format!(
                "context_window must be a positive integer (got {w}) — 0 is a typo, \
                 and a zero denominator makes every usage gauge garbage"
            )));
        }
        let t = &row.tunables;
        validate_tunables(t.max_tokens, t.temperature, t.top_p, t.thinking_budget, t.thinking_style.as_deref())?;
        self.conn.execute(
            "INSERT INTO backend_models (backend_id, model_id, context_window, extra,
                 max_tokens, temperature, top_p, effort, thinking_budget, thinking_style)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(backend_id, model_id) DO UPDATE SET
                 context_window = excluded.context_window,
                 extra = excluded.extra,
                 max_tokens = excluded.max_tokens,
                 temperature = excluded.temperature,
                 top_p = excluded.top_p,
                 effort = excluded.effort,
                 thinking_budget = excluded.thinking_budget,
                 thinking_style = excluded.thinking_style",
            params![
                blob_param(row.backend_id.as_bytes()),
                row.model_id,
                row.context_window,
                row.extra,
                t.max_tokens,
                t.temperature,
                t.top_p,
                t.effort,
                t.thinking_budget,
                t.thinking_style,
            ],
        )?;
        Ok(())
    }

    /// Remove one backend's per-model metadata row. Returns whether a row went.
    pub fn delete_backend_model(
        &self,
        backend_id: BackendId,
        model_id: &str,
    ) -> KernelDbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM backend_models WHERE backend_id = ?1 AND model_id = ?2",
            params![blob_param(backend_id.as_bytes()), model_id],
        )?;
        Ok(n > 0)
    }

    /// One backend's per-model metadata rows, ordered by model id.
    pub fn list_backend_models(
        &self,
        backend_id: BackendId,
    ) -> KernelDbResult<Vec<BackendModelRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT model_id, context_window, extra, max_tokens, temperature, top_p,
                    effort, thinking_budget, thinking_style
             FROM backend_models WHERE backend_id = ?1 ORDER BY model_id",
        )?;
        let rows = stmt.query_map(params![blob_param(backend_id.as_bytes())], |row| {
            Ok(BackendModelRow {
                backend_id,
                model_id: row.get(0)?,
                context_window: row.get(1)?,
                extra: row.get(2)?,
                tunables: ModelTunablesRow {
                    max_tokens: row.get(3)?,
                    temperature: row.get(4)?,
                    top_p: row.get(5)?,
                    effort: row.get(6)?,
                    thinking_budget: row.get(7)?,
                    thinking_style: row.get(8)?,
                },
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ── character slots ─────────────────────────────────────────────────

    /// Insert or replace one slot of a character.
    pub fn set_character_slot(&self, row: &CharacterSlotRow) -> KernelDbResult<()> {
        if row.role.trim().is_empty() {
            return Err(KernelDbError::Validation("role must not be empty".into()));
        }
        if row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("model must not be empty".into()));
        }
        validate_tunables(
            row.max_tokens,
            row.temperature,
            row.top_p,
            row.thinking_budget,
            row.thinking_style.as_deref(),
        )?;
        self.conn.execute(
            &format!(
                "INSERT INTO character_slots ({CHARACTER_SLOT_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(principal_id, role) DO UPDATE SET
                     backend_id = excluded.backend_id,
                     model = excluded.model,
                     max_tokens = excluded.max_tokens,
                     temperature = excluded.temperature,
                     top_p = excluded.top_p,
                     effort = excluded.effort,
                     thinking_budget = excluded.thinking_budget,
                     thinking_style = excluded.thinking_style"
            ),
            params![
                blob_param(row.principal_id.as_bytes()),
                row.role,
                blob_param(row.backend_id.as_bytes()),
                row.model,
                row.max_tokens,
                row.temperature,
                row.top_p,
                row.effort,
                row.thinking_budget,
                row.thinking_style,
            ],
        )?;
        Ok(())
    }

    /// Remove one slot. Returns whether a row went.
    pub fn delete_character_slot(&self, principal_id: PrincipalId, role: &str) -> KernelDbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM character_slots WHERE principal_id = ?1 AND role = ?2",
            params![blob_param(principal_id.as_bytes()), role],
        )?;
        Ok(n > 0)
    }

    /// One character's slots, ordered by role.
    pub fn list_character_slots(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<CharacterSlotRow>> {
        let sql = format!("SELECT {CHARACTER_SLOT_COLS} FROM character_slots WHERE principal_id = ?1 ORDER BY role");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], row_to_character_slot)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// One character's slot for `role`, if it has one.
    pub fn get_character_slot(&self, principal_id: PrincipalId, role: &str) -> KernelDbResult<Option<CharacterSlotRow>> {
        let sql = format!("SELECT {CHARACTER_SLOT_COLS} FROM character_slots WHERE principal_id = ?1 AND role = ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_row(params![blob_param(principal_id.as_bytes()), role], row_to_character_slot)
            .optional()?)
    }

    // ── character models ────────────────────────────────────────────────

    /// Insert or replace a character's model. A root character runs no model,
    /// so it is refused.
    pub fn set_character_model(&self, row: &CharacterModelRow) -> KernelDbResult<()> {
        let Some(sheet) = self.get_character(row.principal_id)? else {
            return Err(KernelDbError::NotFound(format!("character {}", row.principal_id.short())));
        };
        if sheet.root {
            return Err(KernelDbError::Validation(format!(
                "{} is a root character, and a root runs no model",
                sheet.name
            )));
        }
        if row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("model must not be empty".into()));
        }
        validate_tunables(
            row.max_tokens,
            row.temperature,
            row.top_p,
            row.thinking_budget,
            row.thinking_style.as_deref(),
        )?;
        self.conn.execute(
            &format!(
                "INSERT INTO character_models ({CHARACTER_MODEL_COLS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(principal_id) DO UPDATE SET
                     backend_id = excluded.backend_id,
                     model = excluded.model,
                     max_tokens = excluded.max_tokens,
                     temperature = excluded.temperature,
                     top_p = excluded.top_p,
                     effort = excluded.effort,
                     thinking_budget = excluded.thinking_budget,
                     thinking_style = excluded.thinking_style"
            ),
            params![
                blob_param(row.principal_id.as_bytes()),
                blob_param(row.backend_id.as_bytes()),
                row.model,
                row.max_tokens,
                row.temperature,
                row.top_p,
                row.effort,
                row.thinking_budget,
                row.thinking_style,
            ],
        )?;
        Ok(())
    }

    /// Remove a character's model. Returns whether a row went.
    pub fn delete_character_model(&self, principal_id: PrincipalId) -> KernelDbResult<bool> {
        let n = self.conn.execute(
            "DELETE FROM character_models WHERE principal_id = ?1",
            params![blob_param(principal_id.as_bytes())],
        )?;
        Ok(n > 0)
    }

    /// A character's model, if it has one.
    pub fn get_character_model(&self, principal_id: PrincipalId) -> KernelDbResult<Option<CharacterModelRow>> {
        let sql = format!("SELECT {CHARACTER_MODEL_COLS} FROM character_models WHERE principal_id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_row(params![blob_param(principal_id.as_bytes())], row_to_character_model)
            .optional()?)
    }

    // ── characters ──────────────────────────────────────────────────────

    /// Create a character's sheet row. Fails loudly on a duplicate name —
    /// the caller (`kj character create`) checks for an existing name first
    /// and returns that row unchanged rather than calling this twice; this
    /// method itself does not dedupe.
    pub fn insert_character(&self, row: &CharacterRow) -> KernelDbResult<()> {
        validate_label(&row.name)?;
        self.conn
            .execute(
                "INSERT INTO characters (principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, context_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    blob_param(row.principal_id.as_bytes()),
                    row.name,
                    row.created_at,
                    row.retired_at,
                    row.handoff_ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                    row.root,
                    row.root_ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                    row.context_type,
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("character name '{}' already in use", row.name))
            })?;
        Ok(())
    }

    /// Fetch a character by its kernel-owned name.
    pub fn get_character_by_name(&self, name: &str) -> KernelDbResult<Option<CharacterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, context_type
             FROM characters WHERE name = ?1",
        )?;
        Ok(stmt.query_row(params![name], row_to_character_row).optional()?)
    }

    /// Fetch a character by the principal id that plays it — the counterpart
    /// to `get_character_by_name` for callers that only have a context row's
    /// `played_by` (e.g. resolving the name to display in `kj context info`).
    pub fn get_character(&self, principal_id: PrincipalId) -> KernelDbResult<Option<CharacterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, context_type
             FROM characters WHERE principal_id = ?1",
        )?;
        Ok(stmt
            .query_row(params![blob_param(principal_id.as_bytes())], row_to_character_row)
            .optional()?)
    }

    /// Every character, ordered by name. `include_retired` controls whether
    /// a retired row is included — `kj character list` wants only the live
    /// ones by default.
    pub fn list_characters(&self, include_retired: bool) -> KernelDbResult<Vec<CharacterRow>> {
        let sql = if include_retired {
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, context_type \
             FROM characters ORDER BY name"
        } else {
            "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, context_type \
             FROM characters WHERE retired_at IS NULL ORDER BY name"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_character_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Set (or clear) a character's handoff-log context. Called exactly
    /// once per character in the ordinary case, by `kj handoff`'s
    /// get-or-create path, under the same lock that just confirmed the
    /// column was still NULL — see that call site for the race this
    /// guards. `None` is accepted (not just produced) so a future repair
    /// verb can disown a context without a bespoke clear method, mirroring
    /// `update_played_by`.
    pub fn set_character_handoff_ctx(
        &self,
        principal_id: PrincipalId,
        ctx: Option<ContextId>,
    ) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE characters SET handoff_ctx = ?1 WHERE principal_id = ?2",
            params![
                ctx.as_ref().map(|c| c.as_bytes().to_vec()),
                blob_param(principal_id.as_bytes())
            ],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!(
                "character {}",
                principal_id.short()
            )));
        }
        Ok(())
    }

    /// Set or clear a character's `root` flag, which marks a model context
    /// whose kj requests always need approval (`docs/character.md`,
    /// "Roots and rotation").
    pub fn update_character_root(&self, principal_id: PrincipalId, root: bool) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE characters SET root = ?1 WHERE principal_id = ?2",
            params![root, blob_param(principal_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("character {}", principal_id.short())));
        }
        Ok(())
    }

    /// Set or clear the context type a character usually plays.
    pub fn update_character_context_type(&self, principal_id: PrincipalId, context_type: Option<&str>) -> KernelDbResult<()> {
        let updated = self.conn.execute(
            "UPDATE characters SET context_type = ?1 WHERE principal_id = ?2",
            params![context_type, blob_param(principal_id.as_bytes())],
        )?;
        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("character {}", principal_id.short())));
        }
        Ok(())
    }

    /// Retire a character: stamp `retired_at`, first time only. Returns
    /// `true` if this call newly retired it, `false` if it was unknown or
    /// already retired — idempotent re-retire is a no-op success, never an
    /// error, since "already retired" and "just retired" leave the same
    /// row behind.
    ///
    /// Does not touch the character's contexts — `kj character retire`
    /// concludes and archives them separately, through the same
    /// `archive_context`/`conclude_context` every other archival path uses.
    /// A context whose senpai's holder has retired refuses reviewer
    /// resolution by name rather than being skipped ([`Self::route_review`]).
    pub fn retire_character(&self, principal_id: PrincipalId, at: i64) -> KernelDbResult<bool> {
        let updated = self.conn.execute(
            "UPDATE characters SET retired_at = ?1
             WHERE principal_id = ?2 AND retired_at IS NULL",
            params![at, blob_param(principal_id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Resolve a principal to the name a player reads: the two sentinels
    /// first, then the sheet. A principal with a credential but no
    /// `characters` row — the wipe-and-rebind case, or a stranger nobody
    /// minted a sheet for — renders as its id's short form: loud, never
    /// blank, and never a fabricated name (`docs/character.md`, "`auth.db`
    /// is a keyring").
    pub fn name_for(&self, principal_id: PrincipalId) -> String {
        if principal_id == PrincipalId::system() {
            return "system".to_string();
        }
        if principal_id == PrincipalId::beat() {
            return "beat".to_string();
        }
        match self.get_character(principal_id) {
            Ok(Some(row)) => row.name,
            _ => principal_id.short(),
        }
    }

    /// Every live (non-archived) context `principal_id` performs — the set
    /// `kj character retire` archives.
    pub fn contexts_played_by(&self, principal_id: PrincipalId) -> KernelDbResult<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            concat!("SELECT context_id, label,
                    system_prompt, context_state, context_type,
                    created_at, created_by, forked_from, fork_kind,
                    archived_at, workspace_id, preset_id, concluded_at,
                    last_activity_at, promoted_at, demoted_at, paused_at,
             origin_host, ", performer_sql!("contexts"), " AS played_by
             FROM contexts
             WHERE context_id IN (SELECT context_id FROM context_characters WHERE principal_id = ?1)
               AND archived_at IS NULL"),
        )?;
        let rows = stmt.query_map(params![blob_param(principal_id.as_bytes())], row_to_context_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ── model_aliases ───────────────────────────────────────────────────

    /// Insert or replace a `--model` alias.
    pub fn set_model_alias(&self, row: &ModelAliasRow) -> KernelDbResult<()> {
        if row.alias.trim().is_empty() {
            return Err(KernelDbError::Validation("alias must not be empty".into()));
        }
        if row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("model must not be empty".into()));
        }
        self.conn.execute(
            "INSERT INTO model_aliases (alias, backend_id, model) VALUES (?1, ?2, ?3)
             ON CONFLICT(alias) DO UPDATE SET
                 backend_id = excluded.backend_id,
                 model = excluded.model",
            params![
                row.alias,
                blob_param(row.backend_id.as_bytes()),
                row.model
            ],
        )?;
        Ok(())
    }

    /// Remove an alias. Returns whether a row went.
    pub fn delete_model_alias(&self, alias: &str) -> KernelDbResult<bool> {
        let n = self
            .conn
            .execute("DELETE FROM model_aliases WHERE alias = ?1", params![alias])?;
        Ok(n > 0)
    }

    /// Every alias, ordered by name.
    pub fn list_model_aliases(&self) -> KernelDbResult<Vec<ModelAliasRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT alias, backend_id, model FROM model_aliases ORDER BY alias")?;
        let rows = stmt.query_map([], |row| {
            Ok(ModelAliasRow {
                alias: row.get(0)?,
                backend_id: read_backend_id(row, 1)?,
                model: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    // ── embedding_config ────────────────────────────────────────────────

    /// The singleton embedding row, or `None` when never seeded.
    pub fn get_embedding_config(&self) -> KernelDbResult<Option<EmbeddingConfigRow>> {
        Ok(self.conn.query_row(
            "SELECT enabled, backend, model, dimensions, profile, timeout_ms, max_in_flight, max_context_bytes
             FROM embedding_config WHERE id = 1",
            [], |row| Ok(EmbeddingConfigRow {
                enabled: row.get::<_, i64>(0)? != 0, backend: row.get(1)?, model: row.get(2)?,
                dimensions: row.get(3)?, profile: row.get(4)?,
                timeout_ms: row.get(5)?, max_in_flight: row.get(6)?, max_context_bytes: row.get(7)?,
            }),
        ).optional()?)
    }

    /// Replace the singleton embedding service configuration.
    pub fn set_embedding_config(&self, row: &EmbeddingConfigRow) -> KernelDbResult<()> {
        if row.backend.trim().is_empty() || row.model.trim().is_empty() {
            return Err(KernelDbError::Validation("embedding backend and model must be nonempty".into()));
        }
        if row.dimensions <= 0 || row.timeout_ms <= 0 || row.max_in_flight <= 0 || row.max_context_bytes <= 0 {
            return Err(KernelDbError::Validation("embedding dimensions, timeout_ms, max_in_flight and max_context_bytes must be positive".into()));
        }
        row.profile.parse::<kaijutsu_index::EmbeddingProfile>().map_err(KernelDbError::Validation)?;
        self.conn.execute(
            "INSERT INTO embedding_config (id, enabled, backend, model, dimensions, profile, timeout_ms, max_in_flight, max_context_bytes)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled, backend = excluded.backend,
                 model = excluded.model, dimensions = excluded.dimensions, profile = excluded.profile,
                 timeout_ms = excluded.timeout_ms, max_in_flight = excluded.max_in_flight,
                 max_context_bytes = excluded.max_context_bytes",
            params![row.enabled as i64, row.backend, row.model, row.dimensions, row.profile,
                row.timeout_ms, row.max_in_flight, row.max_context_bytes],
        )?;
        Ok(())
    }
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

    #[test]
    fn publication_handoffs_gain_abandonment_on_existing_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        {
            let db = KernelDb::open(&path).unwrap();
            db.conn.execute_batch("ALTER TABLE approval_pair_handoffs DROP COLUMN abandoned_reason").unwrap();
        }
        let db = KernelDb::open(&path).unwrap();
        assert!(db.approval_pair_abandoned_reason("missing").unwrap().is_none());
        drop(db);
        assert!(KernelDb::open(&path).unwrap().unpublished_approval_pairs().unwrap().is_empty());
    }

    /// A database whose `backend_models` predates the tunable columns opens,
    /// keeps its rows with every tunable unset, and stores tunables after.
    #[test]
    fn backend_models_gain_tunables_on_existing_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        let backend_id = BackendId::new();
        {
            let db = KernelDb::open(&path).unwrap();
            db.conn.execute_batch(
                "DROP TABLE backend_models;
                 CREATE TABLE backend_models (
                     backend_id BLOB NOT NULL REFERENCES backends(backend_id) ON DELETE CASCADE,
                     model_id TEXT NOT NULL,
                     context_window INTEGER CHECK (context_window IS NULL OR context_window > 0),
                     extra TEXT,
                     PRIMARY KEY (backend_id, model_id));",
            ).unwrap();
            db.conn.execute(
                "INSERT INTO backends (backend_id, name, kind, key_optional, created_at, created_by)
                 VALUES (?1, 'old', 'openai', 1, 0, ?2)",
                params![blob_param(backend_id.as_bytes()), blob_param(PrincipalId::system().as_bytes())],
            ).unwrap();
            db.conn.execute(
                "INSERT INTO backend_models (backend_id, model_id, context_window) VALUES (?1, 'm', 1000)",
                params![blob_param(backend_id.as_bytes())],
            ).unwrap();
        }
        let db = KernelDb::open(&path).unwrap();
        let rows = db.list_backend_models(backend_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].context_window, Some(1000));
        assert_eq!(rows[0].tunables, ModelTunablesRow::default());
        let mut row = rows[0].clone();
        row.tunables.max_tokens = Some(65536);
        row.tunables.effort = Some("high".into());
        db.set_backend_model(&row).unwrap();
        assert_eq!(db.list_backend_models(backend_id).unwrap()[0].tunables.max_tokens, Some(65536));
        assert!(db.conn.execute(
            "UPDATE backend_models SET max_tokens = 0 WHERE model_id = 'm'", [],
        ).is_err(), "the migrated column carries its CHECK");
    }

    fn embedding_row() -> EmbeddingConfigRow {
        EmbeddingConfigRow {
            enabled: true, backend: "openrouter".into(), model: "qwen/qwen3-embedding-8b".into(),
            dimensions: 1024, profile: "qwen3".into(),
            timeout_ms: 30_000, max_in_flight: 2, max_context_bytes: 2048,
        }
    }

    /// Both older shapes (a builtin model directory, then a service endpoint)
    /// name no backend and the kernel ships no default, so the rebuild leaves
    /// the table empty: the semantic index stays off until an operator names
    /// a backend and model.
    #[test]
    fn legacy_embedding_config_migrates_to_an_unconfigured_service() {
        for (shape, legacy) in [
            ("builtin model", "CREATE TABLE embedding_config (id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL,
                model_dir TEXT NOT NULL, dimensions INTEGER NOT NULL, max_tokens INTEGER NOT NULL);
             INSERT INTO embedding_config VALUES (1, 1, '/old/model', 384, 123);"),
            ("service endpoint", "CREATE TABLE embedding_config (id INTEGER PRIMARY KEY, enabled INTEGER NOT NULL,
                endpoint TEXT NOT NULL, timeout_ms INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
                max_context_bytes INTEGER NOT NULL);
             INSERT INTO embedding_config VALUES (1, 1, 'http://embedder.example:8088', 30000, 2, 2048);"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("kernel.db");
            {
                let db = KernelDb::open(&path).unwrap();
                db.conn.execute_batch(&format!("DROP TABLE embedding_config; {legacy}")).unwrap();
            }
            let db = KernelDb::open(&path).unwrap();
            assert_eq!(db.get_embedding_config().unwrap(), None, "{shape}");
            db.set_embedding_config(&embedding_row()).unwrap();
            drop(db);
            assert_eq!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap(), Some(embedding_row()), "{shape}");
        }
    }

    /// The old-shape test is whether the table lacks `backend`, not whether it
    /// carries an old column: a table with the new columns keeps its row.
    #[test]
    fn a_table_with_the_new_columns_keeps_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel.db");
        {
            let db = KernelDb::open(&path).unwrap();
            db.set_embedding_config(&embedding_row()).unwrap();
            db.conn.execute_batch("ALTER TABLE embedding_config ADD COLUMN endpoint TEXT;").unwrap();
        }
        assert_eq!(KernelDb::open(&path).unwrap().get_embedding_config().unwrap(), Some(embedding_row()));
    }

    #[test]
    fn an_embedding_row_is_refused_when_it_cannot_work() {
        let db = KernelDb::temporary().unwrap();
        for (what, row) in [
            ("empty backend", EmbeddingConfigRow { backend: " ".into(), ..embedding_row() }),
            ("empty model", EmbeddingConfigRow { model: String::new(), ..embedding_row() }),
            ("zero dimensions", EmbeddingConfigRow { dimensions: 0, ..embedding_row() }),
            ("zero timeout", EmbeddingConfigRow { timeout_ms: 0, ..embedding_row() }),
            ("zero concurrency", EmbeddingConfigRow { max_in_flight: 0, ..embedding_row() }),
            ("zero projection", EmbeddingConfigRow { max_context_bytes: 0, ..embedding_row() }),
        ] {
            assert!(db.set_embedding_config(&row).is_err(), "accepted: {what}");
        }
        let error = db.set_embedding_config(&EmbeddingConfigRow { profile: "gemini".into(), ..embedding_row() })
            .unwrap_err().to_string();
        assert!(error.contains("plain") && error.contains("qwen3") && error.contains("embeddinggemma"),
            "an unknown profile names the valid ones: {error}");
        assert_eq!(db.get_embedding_config().unwrap(), None, "a refusal writes nothing");
    }

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

    /// `origin_host` is deliberately NOT part of `write_context`'s own
    /// INSERT (see the field's doc comment) — it is set via the dedicated
    /// `set_origin_host`, exactly like `update_label`. Round-trip through
    /// both: a context created with no host known, then stamped after the
    /// fact, ends up with the value `get_context` reads back — and does NOT
    /// silently vanish or get coerced to empty-string (which would break the
    /// "-" display fallback, since `Some("")` and `None` render identically
    /// but mean different things to a caller checking `.is_some()`).
    #[test]
    fn set_origin_host_round_trips_through_get_context() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("origin-host-set"));
        insert_context_with_doc(&db, &row, ws_id);

        // Freshly created: no host reported yet.
        assert_eq!(db.get_context(row.context_id).unwrap().unwrap().origin_host, None);

        db.set_origin_host(row.context_id, Some("zorak")).unwrap();
        assert_eq!(
            db.get_context(row.context_id).unwrap().unwrap().origin_host,
            Some("zorak".to_string()),
            "set_origin_host must be visible on the very next get_context read"
        );

        // Clearing (host = None) must also round-trip, not merely no-op.
        db.set_origin_host(row.context_id, None).unwrap();
        assert_eq!(
            db.get_context(row.context_id).unwrap().unwrap().origin_host,
            None,
            "set_origin_host(None) must clear a previously-set value"
        );
    }

    /// `set_origin_host` on an unknown context must fail loudly
    /// (`NotFound`), same convention as `update_label`/`update_context_type`
    /// — never a silent no-op that could be mistaken for success.
    #[test]
    fn set_origin_host_unknown_context_is_not_found() {
        let db = KernelDb::temporary().unwrap();
        let err = db.set_origin_host(ContextId::new(), Some("moltar")).unwrap_err();
        assert!(matches!(err, KernelDbError::NotFound(_)), "got: {err:?}");
    }

    /// `list_active_contexts`/`list_all_contexts`/`fork_children`/
    /// `context_dag`/`fork_lineage`/`find_context_by_label` all share the
    /// same `contexts` column list — `context_dag` must carry `origin_host`
    /// through to its rows.
    #[test]
    fn origin_host_survives_the_context_dag_cte_read() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("origin-host-dag"));
        insert_context_with_doc(&db, &row, ws_id);
        db.set_origin_host(row.context_id, Some("moltar")).unwrap();

        let dag = db.context_dag().unwrap();
        let (found, depth) = dag
            .into_iter()
            .find(|(r, _)| r.context_id == row.context_id)
            .expect("row must appear in the DAG (it's a root — no forked_from)");
        assert_eq!(found.origin_host, Some("moltar".to_string()));
        assert_eq!(depth, 0, "a root with no parent is depth 0");
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

    /// `touch_context_activity` stamps `last_activity_at`, and pre-touch rows
    /// read back `None` (not silently defaulted) — the list SELECTs are what
    /// backfill via COALESCE, not this row-level accessor.
    #[test]
    fn touch_context_activity_stamps_and_advances() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("touch-me"));
        let cid = row.context_id;
        let created_at = row.created_at;
        insert_context_with_doc(&db, &row, ws_id);

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.last_activity_at, None);

        let t0 = created_at + 1000;
        db.touch_context_activity(cid, t0).unwrap();
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.last_activity_at, Some(t0));
        assert!(loaded.last_activity_at.unwrap() > created_at);

        // A second touch advances further (not a first-write-wins latch like
        // concluded_at — activity is repeatedly re-stamped).
        let t1 = t0 + 1000;
        db.touch_context_activity(cid, t1).unwrap();
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.last_activity_at, Some(t1));
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

    #[test]
    fn context_lifecycle() {
        let db = KernelDb::temporary().unwrap();
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

    fn pending_ask_for_context(db: &KernelDb, context: ContextId) -> String {
        let principal = PrincipalId::new();
        db.create_approval_ask(&approval_ledger::types::NewAsk {
            context_id: context.as_bytes().to_vec(),
            actor_id: principal.as_bytes().to_vec(),
            reviewer_id: PrincipalId::new().as_bytes().to_vec(),
            principal_id: principal.as_bytes().to_vec(),
            origin: approval_ledger::types::Origin::KjVerb,
            instance: None,
            tool: None,
            hook_id: None,
            description: "archive sweep test".into(),
            statements: vec![],
            authorized_label: None,
            rc_run_id: None,
            expires_at: None,
            options: vec![],
            signals: vec![],
            cwd: None,
            exec_source: None,
            exec_stdin: None,
            continuation_epoch: None,
            env: vec![],
        }, false).unwrap()
    }

    fn approval_status(db: &KernelDb, request: &str) -> approval_ledger::types::ApprovalStatus {
        db.get_approval(request).unwrap().expect("ask exists").status
    }

    #[test]
    fn archive_context_rolls_back_when_its_ask_sweep_fails() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("archive-sweep-rollback"));
        let context = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        let request = pending_ask_for_context(&db, context);
        db.conn.execute_batch(
            "CREATE TRIGGER reject_archive_ask_sweep BEFORE UPDATE OF status ON approvals
             WHEN NEW.status = 'abandoned'
             BEGIN SELECT RAISE(ABORT, 'injected archive ask sweep failure'); END;",
        ).unwrap();

        let error = db.archive_context(context).expect_err("the failed sweep must roll back archive");
        assert!(error.to_string().contains("injected archive ask sweep failure"), "{error}");
        let loaded = db.get_context(context).unwrap().unwrap();
        assert!(!loaded.is_archived(), "state change must roll back with its failed sweep");
        assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Pending);

        db.conn.execute("DROP TRIGGER reject_archive_ask_sweep", []).unwrap();
        assert!(db.archive_context(context).unwrap(), "retry archives after the sweep fault clears");
        assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Abandoned);
    }

    #[test]
    fn archive_context_rolls_back_inside_a_committed_outer_transaction() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("archive-sweep-nested-rollback"));
        let context = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        let request = pending_ask_for_context(&db, context);
        db.conn.execute_batch(
            "CREATE TRIGGER reject_nested_archive_ask_sweep BEFORE UPDATE OF status ON approvals
             WHEN NEW.status = 'abandoned'
             BEGIN SELECT RAISE(ABORT, 'injected nested archive ask sweep failure'); END;",
        ).unwrap();

        db.in_transaction(|db| {
            assert!(db.archive_context(context).is_err(), "the nested archive must report its failed sweep");
            Ok(())
        }).unwrap();

        let loaded = db.get_context(context).unwrap().unwrap();
        assert!(!loaded.is_archived(), "the caught archive error must not leak its state change into the outer commit");
        assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Pending);
    }

    #[test]
    fn archive_context_retries_the_sweep_for_an_already_archived_context() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("archive-sweep-heal"));
        let context = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        let request = pending_ask_for_context(&db, context);
        db.conn.execute(
            "UPDATE contexts SET archived_at = ?1, context_state = 'archived' WHERE context_id = ?2",
            params![now_millis(), blob_param(context.as_bytes())],
        ).unwrap();

        assert!(!db.archive_context(context).unwrap(), "the historical state transition was already applied");
        assert_eq!(approval_status(&db, &request), approval_ledger::types::ApprovalStatus::Abandoned);
    }

    #[test]
    fn conclude_context_sets_state_timestamp_and_is_idempotent() {
        let db = KernelDb::temporary().unwrap();
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
        let db = KernelDb::temporary().unwrap();
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
        let db = KernelDb::temporary().unwrap();
        let row = make_context_row(Some("my:label"));

        let err = db.insert_context(&row).unwrap_err();
        assert!(matches!(err, KernelDbError::InvalidLabel(_)));
    }

    // ── 4. Label uniqueness ────────────────────────────────────────────

    #[test]
    fn label_uniqueness() {
        let db = KernelDb::temporary().unwrap();
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
        let db = KernelDb::temporary().unwrap();
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

    // ── 8. Drift edge allows dupes ─────────────────────────────────────

    #[test]
    fn drift_edge_allows_dupes() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(None);
        let b = make_context_row(None);
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        // Multiple drift edges same pair → all stored
        for _ in 0..3 {
            db.insert_edge(&make_edge(a.context_id, b.context_id))
                .unwrap();
        }

        let edges = db.edges_from(a.context_id).unwrap();
        assert_eq!(edges.len(), 3);
    }

    // ── 10. Preset lifecycle ───────────────────────────────────────────

    // ── 11. Workspace lifecycle ────────────────────────────────────────

    // ── 12. Workspace soft delete ──────────────────────────────────────

    // ── 13. Drift push creates edge ────────────────────────────────────

    #[test]
    fn drift_push_creates_edge() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(Some("source"));
        let b = make_context_row(Some("target"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        // Simulate drift push → create drift edge
        let edge1 = make_edge(a.context_id, b.context_id);
        db.insert_edge(&edge1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1));

        let edge2 = make_edge(a.context_id, b.context_id);
        db.insert_edge(&edge2).unwrap();

        // edges_from returns both
        let edges = db.edges_from(a.context_id).unwrap();
        assert_eq!(edges.len(), 2);

        // drift_provenance returns newest first
        let prov = db.drift_provenance(a.context_id).unwrap();
        assert_eq!(prov.len(), 2);
        assert!(prov[0].created_at >= prov[1].created_at);
    }

    // ── 14. Context DAG recursive ──────────────────────────────────────

    #[test]
    fn context_dag_recursive() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        // Create a 5-node tree: root → [a, b], a → [c, d]
        let root = make_context_row(Some("root"));
        let mut a = make_context_row(Some("a"));
        let mut b = make_context_row(Some("b"));
        let mut c = make_context_row(Some("c"));
        let mut d = make_context_row(Some("d"));
        a.forked_from = Some(root.context_id);
        b.forked_from = Some(root.context_id);
        c.forked_from = Some(a.context_id);
        d.forked_from = Some(a.context_id);

        for ctx in [&root, &a, &b, &c, &d] {
            insert_context_with_doc(&db, ctx, ws_id);
        }

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

    #[test]
    fn context_dag_lists_every_child_under_its_own_parent() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        // root has two children; the earlier one has a child of its own that
        // is newer than the later sibling.
        let base = now_millis();
        let mut root = make_context_row(Some("root"));
        let mut first = make_context_row(Some("first"));
        let mut later = make_context_row(Some("later"));
        let mut grandchild = make_context_row(Some("grandchild"));
        root.created_at = base;
        first.created_at = base + 1;
        later.created_at = base + 2;
        grandchild.created_at = base + 3;
        first.forked_from = Some(root.context_id);
        later.forked_from = Some(root.context_id);
        grandchild.forked_from = Some(first.context_id);
        for ctx in [&root, &first, &later, &grandchild] {
            insert_context_with_doc(&db, ctx, ws_id);
        }

        let dag = db.context_dag().unwrap();
        let shape: Vec<(String, i64)> = dag
            .iter()
            .map(|(c, depth)| (c.label.clone().unwrap(), *depth))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("root".to_string(), 0),
                ("first".to_string(), 1),
                ("grandchild".to_string(), 2),
                ("later".to_string(), 1),
            ],
            "each child follows its own parent, depth-first"
        );
    }

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

    #[test]
    fn tag_resolution() {
        let db = KernelDb::temporary().unwrap();
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
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let ctx = make_context_row(Some("unique-label"));
        insert_context_with_doc(&db, &ctx, ws_id);

        // Exact label
        let r = db.resolve_context("unique-label").unwrap();
        assert_eq!(r, ctx.context_id);

        // Label prefix
        let r = db.resolve_context("unique").unwrap();
        assert_eq!(r, ctx.context_id);

        // Short id (entropy tail)
        let r = db.resolve_context(&ctx.context_id.short()).unwrap();
        assert_eq!(r, ctx.context_id);

        // Not found
        let err = db.resolve_context("nonexistent").unwrap_err();
        assert!(matches!(err, KernelDbError::NotFound(_)));
    }

    // ── 17. Null labels coexist ────────────────────────────────────────

    #[test]
    fn null_labels_coexist() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        // Insert 5 contexts with NULL label — all succeed
        for _ in 0..5 {
            let row = make_context_row(None);
            insert_context_with_doc(&db, &row, ws_id);
        }

        let all = db.list_all_contexts().unwrap();
        assert_eq!(all.len(), 5);
    }

    // ── 18. Archive excludes from active + fork_children ─────────

    #[test]
    fn archive_excludes_from_active() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let parent = make_context_row(Some("parent"));
        let mut child = make_context_row(Some("child"));
        child.forked_from = Some(parent.context_id);
        insert_context_with_doc(&db, &parent, ws_id);
        insert_context_with_doc(&db, &child, ws_id);

        // Before archive
        let children = db.fork_children(parent.context_id).unwrap();
        assert_eq!(children.len(), 1);

        // Archive child
        db.archive_context(child.context_id).unwrap();

        // list_active excludes
        let active = db.list_active_contexts().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].context_id, parent.context_id);

        // fork_children excludes archived
        let children = db.fork_children(parent.context_id).unwrap();
        assert!(children.is_empty());
    }

    // ── 19. Context edge cascade ───────────────────────────────────────

    #[test]
    fn context_edge_cascade() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let a = make_context_row(Some("edge-a"));
        let b = make_context_row(Some("edge-b"));
        insert_context_with_doc(&db, &a, ws_id);
        insert_context_with_doc(&db, &b, ws_id);

        db.insert_edge(&make_edge(a.context_id, b.context_id))
            .unwrap();

        // The edge exists
        let all = db.edges_from(a.context_id).unwrap();
        assert_eq!(all.len(), 1);

        // Delete context a → edges cascade
        db.conn
            .execute(
                "DELETE FROM contexts WHERE context_id = ?1",
                params![blob_param(a.context_id.as_bytes())],
            )
            .unwrap();

        // Edges gone
        let all = db.edges_from(a.context_id).unwrap();
        assert!(all.is_empty());
    }

    // ── 20. Context row to_context conversion ──────────────────────────

    #[test]
    fn context_row_to_context() {
        let creator = PrincipalId::new();
        let parent = ContextId::new();
        let now = now_millis();

        let row = ContextRow {
            context_id: ContextId::new(),
            label: Some("test".into()),
            system_prompt: None,
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
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            origin_host: None,
            played_by: None,
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
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let creator = PrincipalId::new();
        let parent_id = ContextId::new();

        // Insert parent first (forked_from FK requires it to exist)
        let parent = ContextRow {
            context_id: parent_id,
            label: Some("parent".into()),
            system_prompt: Some("You are helpful.".into()),
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
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            origin_host: None,
            played_by: None,
        };
        insert_context_with_doc(&db, &parent, ws_id);

        // Insert child forked from parent.
        let child_id = ContextId::new();
        let child = ContextRow {
            context_id: child_id,
            label: Some("child-fork".into()),
            system_prompt: Some("Be concise.".into()),
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
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            origin_host: None,
            played_by: None,
        };
        insert_context_with_doc(&db, &child, ws_id);

        // Read back and verify all 16 fields
        let recovered = db.get_context(child_id).unwrap().expect("child not found");
        assert_eq!(recovered.context_id, child_id);
        assert_eq!(recovered.label, Some("child-fork".into()));
        assert_eq!(recovered.system_prompt, Some("Be concise.".into()));
        assert_eq!(recovered.created_at, 2000);
        assert_eq!(recovered.created_by, creator);
        assert_eq!(recovered.forked_from, Some(parent_id));
        assert_eq!(recovered.fork_kind, Some(ForkKind::Full));
        assert!(recovered.archived_at.is_none());
        assert!(recovered.workspace_id.is_none());
        assert!(recovered.preset_id.is_none());
        assert_eq!(recovered.played_by, None, "NULL played_by preserves today's behavior");

        // Verify list_active_contexts returns both
        let active = db.list_active_contexts().unwrap();
        assert_eq!(active.len(), 2);

        // Verify to_context() preserves forked_from
        let ctx = recovered.to_context();
        assert_eq!(ctx.forked_from, Some(parent_id));
        assert_eq!(ctx.created_by, creator);

        // A fixture's performer is the principal of the context's context
        // character.
        let character = PrincipalId::new();
        db.update_played_by(child_id, Some(character)).unwrap();
        let played = db.get_context(child_id).unwrap().unwrap();
        assert_eq!(played.played_by, Some(character));
        let context_character = db.context_character(child_id).unwrap().expect("a performed context has a context character");
        assert_eq!(context_character.principal_id, character);
        db.update_played_by(child_id, Some(character)).unwrap();
        assert_eq!(db.context_character(child_id).unwrap().unwrap(), context_character, "the same character changes nothing");
        assert!(db.update_played_by(child_id, None).is_err(), "a performer cannot be cleared");
        assert_eq!(db.get_context(child_id).unwrap().unwrap().played_by, Some(character));
    }

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

    /// Create, fetch by name, fetch by principal, list, and retire — the
    /// full lifecycle a sheet row goes through in this slice.
    #[test]
    fn character_crud_roundtrip() {
        let db = KernelDb::temporary().unwrap();
        let principal = PrincipalId::new();
        db.insert_character(&CharacterRow { context_type: None,
            principal_id: principal,
            name: "hajime".to_string(),
            created_at: 1000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();

        let by_name = db.get_character_by_name("hajime").unwrap().unwrap();
        assert_eq!(by_name.principal_id, principal);
        let by_principal = db.get_character(principal).unwrap().unwrap();
        assert_eq!(by_principal.name, "hajime");

        assert!(db.get_character_by_name("nobody").unwrap().is_none());
        assert!(db.get_character(PrincipalId::new()).unwrap().is_none());

        let live = db.list_characters(false).unwrap();
        assert_eq!(live.len(), 1);

        // Retire: first call newly retires, second is a no-op that still
        // reports "nothing new happened" rather than an error.
        assert!(db.retire_character(principal, 2000).unwrap());
        assert!(!db.retire_character(principal, 3000).unwrap());
        let retired = db.get_character(principal).unwrap().unwrap();
        assert_eq!(retired.retired_at, Some(2000), "first retire wins the timestamp");

        // A retired character drops out of the live-only listing but stays
        // reachable with `include_retired`.
        assert!(db.list_characters(false).unwrap().is_empty());
        assert_eq!(db.list_characters(true).unwrap().len(), 1);
    }

    /// A duplicate name fails loudly rather than minting a second principal
    /// under the same name.
    #[test]
    fn character_duplicate_name_is_rejected() {
        let db = KernelDb::temporary().unwrap();
        db.insert_character(&CharacterRow { context_type: None,
            principal_id: PrincipalId::new(),
            name: "hajime".to_string(),
            created_at: 1000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
        let err = db
            .insert_character(&CharacterRow { context_type: None,
                principal_id: PrincipalId::new(),
                name: "hajime".to_string(),
                created_at: 2000,
                retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            })
            .unwrap_err();
        assert!(matches!(err, KernelDbError::LabelConflict(_)), "got: {err}");
    }

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

    /// `root` round-trips through insert and through
    /// `update_character_root`, in both directions.
    #[test]
    fn character_root_flag_round_trips_in_both_directions() {
        let db = KernelDb::temporary().unwrap();
        let amy = PrincipalId::new();
        let coder = PrincipalId::new();
        db.insert_character(&CharacterRow { context_type: None,
            principal_id: amy, name: "amy".into(), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: true,
        }).unwrap();
        db.insert_character(&CharacterRow { context_type: None,
            principal_id: coder, name: "coder".into(), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        assert!(db.get_character(amy).unwrap().unwrap().root, "a root inserts as a root");
        assert!(!db.get_character(coder).unwrap().unwrap().root);

        db.update_character_root(coder, true).unwrap();
        assert!(db.get_character(coder).unwrap().unwrap().root);
        db.update_character_root(coder, false).unwrap();
        assert!(!db.get_character(coder).unwrap().unwrap().root);

        let error = db.update_character_root(PrincipalId::new(), true).unwrap_err();
        assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");
    }

    // ── 22d. review routing climbs the senpai chain ─────────────────────

    /// amy's root console (played by amy, a root) → banto's seat → a lane
    /// played by coder: the coder's ask resolves to banto, banto's to amy,
    /// and amy's own statement in her console exhausts the climb and becomes
    /// a self-confirmation rather than an unanswerable ask.
    #[test]
    fn route_review_goes_to_the_senpais_holder_and_a_root_console_confirms_itself() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();

        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "lane-a", Some(&seat), Some(coder));

        assert_eq!(
            db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(),
            EffectiveReviewer::Assigned(banto),
            "the lane's senpai is banto's seat, held by banto",
        );
        assert_eq!(
            db.route_review(ReviewQuery::ask(seat.context_id, banto)).unwrap(),
            EffectiveReviewer::Assigned(amy),
            "the seat's senpai is amy's console",
        );
        assert_eq!(
            db.route_review(ReviewQuery::ask(root.context_id, amy)).unwrap(),
            EffectiveReviewer::SelfConfirmation(amy),
            "a root console has no senpai and its root confirms itself",
        );
        for context in [root.context_id, seat.context_id, lane.context_id] {
            assert_eq!(db.lineage_root(context).unwrap(), amy, "amy tops the senpai chain of every context");
        }
    }

    /// A senpai's holder is the character of its instance; a root console is
    /// the conversation of its root's instance with no senpai, and the root's
    /// sheet reads it back as `root_ctx` whatever the stored column says.
    #[test]
    fn a_root_console_is_the_conversation_of_its_roots_instance_with_no_senpai() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, amy] = seed_characters(&db, &["coder", "amy"])[..] else { unreachable!() };
        assert_eq!(db.get_character(amy).unwrap().unwrap().root_ctx, None, "a character that is not a root has no console");
        db.update_character_root(amy, true).unwrap();
        let console = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        assert_eq!(db.get_character(amy).unwrap().unwrap().root_ctx, Some(console.context_id), "the root instance's conversation is the console");
        let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&console), Some(coder));

        assert_eq!(db.context_holder(console.context_id).unwrap(), Some(amy));
        assert_eq!(db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(), EffectiveReviewer::Assigned(amy));
        assert_eq!(db.lineage_root(lane.context_id).unwrap(), amy);
    }

    /// A banto that forks itself answers to the banto's own senpai: the
    /// fork's senpai is banto's seat, held by the actor, so the climb goes on.
    #[test]
    fn route_review_climbs_past_a_senpai_the_actor_holds() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [banto, amy] = seed_characters(&db, &["banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));
        let fork = senpai_ctx_row(&db, ws_id, "banto-fork", Some(&seat), Some(banto));

        assert_eq!(
            db.route_review(ReviewQuery::ask(fork.context_id, banto)).unwrap(),
            EffectiveReviewer::Assigned(amy),
        );
        // A human at the top of the chain, acting in the fork, confirms themself.
        assert_eq!(
            db.route_review(ReviewQuery::ask(fork.context_id, amy)).unwrap(),
            EffectiveReviewer::SelfConfirmation(amy),
            "a fork's performer is its source's holder, so the climb passes banto and then amy herself",
        );
    }

    /// A root confirms its own statements only in the console it holds: in
    /// another root's console it is refused, naming the actor and the context.
    #[test]
    fn a_root_confirms_only_the_console_it_holds() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [amy, bob] = seed_characters(&db, &["amy", "bob"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        db.update_character_root(bob, true).unwrap();
        let amy_console = senpai_ctx_row(&db, ws_id, "amy-console", None, Some(amy));
        let bob_console = senpai_ctx_row(&db, ws_id, "bob-console", None, Some(bob));
        assert_eq!(db.route_review(ReviewQuery::ask(amy_console.context_id, amy)).unwrap(), EffectiveReviewer::SelfConfirmation(amy));
        assert_eq!(db.route_review(ReviewQuery::ask(bob_console.context_id, bob)).unwrap(), EffectiveReviewer::SelfConfirmation(bob));
        let error = db.route_review(ReviewQuery::ask(bob_console.context_id, amy)).unwrap_err();
        assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
        assert!(error.to_string().contains("amy") && error.to_string().contains("bob-console"), "{error}");
    }

    /// Only a root character confirms its own statements. A non-root actor
    /// at a context with no senpai has no reviewer, and routing refuses
    /// naming the actor and the context.
    #[test]
    fn route_review_refuses_a_non_root_actor_at_a_top_context() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [bob, coder] = seed_characters(&db, &["bob", "coder"])[..] else { unreachable!() };

        let orphan = senpai_ctx_row(&db, ws_id, "orphan-top", None, Some(bob));
        let error = db.route_review(ReviewQuery::ask(orphan.context_id, bob)).unwrap_err();
        assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
        let text = error.to_string();
        assert!(text.contains("bob") && text.contains("orphan-top") && text.contains("root"), "{text}");
        assert!(text.contains("kj context move"), "names the fix: {text}");

        // A lane under a top held by the actor itself climbs into the same refusal.
        let lane = senpai_ctx_row(&db, ws_id, "under-bob", Some(&orphan), Some(coder));
        assert_eq!(db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(), EffectiveReviewer::Assigned(bob));
        let same = senpai_ctx_row(&db, ws_id, "bob-again", Some(&orphan), Some(bob));
        let error = db.route_review(ReviewQuery::ask(same.context_id, bob)).unwrap_err().to_string();
        assert!(error.contains("bob") && error.contains("orphan-top"), "{error}");

        let retired_root = seed_characters(&db, &["gone"])[0];
        db.update_character_root(retired_root, true).unwrap();
        assert!(db.retire_character(retired_root, 1).unwrap());
        let abandoned = senpai_ctx_row(&db, ws_id, "abandoned-root", None, Some(retired_root));
        let error = db.route_review(ReviewQuery::ask(abandoned.context_id, retired_root)).unwrap_err();
        assert!(error.to_string().contains("gone"), "a retired root does not confirm itself: {error}");
    }

    /// A non-root character acting in a root context it does not hold is
    /// reviewed by the live root that holds it: the climb starts at the
    /// context itself. A holder that is not a live root does not review, so a
    /// non-root top still refuses, and a root in another root's console is
    /// still refused.
    #[test]
    fn route_review_makes_a_live_root_review_a_model_acting_in_its_context() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [claude, amy, bob] = seed_characters(&db, &["claude", "amy", "bob"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        assert_eq!(
            db.route_review(ReviewQuery::ask(root.context_id, claude)).unwrap(),
            EffectiveReviewer::TopHolder(amy),
            "amy holds the top context, so she reviews what claude does in it",
        );
        let orphan = senpai_ctx_row(&db, ws_id, "orphan-top", None, Some(bob));
        assert!(db.route_review(ReviewQuery::ask(orphan.context_id, claude)).is_err(), "bob is no live root");
        assert!(
            db.route_review(ReviewQuery::ask(root.context_id, claude).above(amy)).is_err(),
            "an escalation past amy has nobody above her",
        );
    }

    /// A retired holder refuses by name; it never substitutes another character.
    #[test]
    fn route_review_retired_holder_refuses_by_name() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "retired-seat", Some(&root), Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "retired-seat-lane", Some(&seat), Some(coder));
        assert!(db.retire_character(banto, 1).unwrap());

        let error = db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap_err().to_string();
        assert!(error.contains("banto") && error.contains("retired"), "{error}");
        assert!(error.contains("retired-seat"), "names the senpai context: {error}");
        assert!(!error.contains("amy"), "never substitutes the next holder up: {error}");
    }

    /// A context with no context character routes nothing: it refuses,
    /// naming the context; a context with no senpai has nobody above it.
    #[test]
    fn route_review_refuses_a_context_with_no_context_character() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder] = seed_characters(&db, &["coder"])[..] else { unreachable!() };
        let bare = senpai_ctx_row(&db, ws_id, "no-context-character", None, None);
        let error = db.route_review(ReviewQuery::ask(bare.context_id, coder)).unwrap_err();
        assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
        let text = error.to_string();
        assert!(text.contains("no-context-character") && text.contains("no context character"), "{text}");
        let lane = senpai_ctx_row(&db, ws_id, "under-bare", None, Some(coder));
        let text = db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap_err().to_string();
        assert!(text.contains("has no senpai above it") || text.contains("nobody reviews"), "a context with no senpai has nobody above it: {text}");
    }

    /// An archived senpai is climbed through (it still answers for what was
    /// done under it); a missing one, or a missing context, is corruption.
    #[test]
    fn route_review_walks_through_archived_and_refuses_a_missing_senpai() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        let seat = senpai_ctx_row(&db, ws_id, "archived-seat", None, Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "archived-seat-lane", Some(&seat), Some(coder));
        db.archive_context(seat.context_id).unwrap();
        assert_eq!(
            db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(),
            EffectiveReviewer::Assigned(banto),
            "an archived senpai is still the one who answers for what was done under it",
        );

        let orphan_senpai = ContextId::new();
        db.conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        db.conn.execute(
            "UPDATE context_characters SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(orphan_senpai.as_bytes()), blob_param(lane.context_id.as_bytes())],
        ).unwrap();
        db.conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        let error = db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap_err();
        assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");

        let missing = db.route_review(ReviewQuery::ask(ContextId::new(), amy)).unwrap_err();
        assert!(matches!(missing, KernelDbError::NotFound(_)), "{missing}");
    }

    /// A senpai chain that loops is corruption; the climb stops with a named
    /// error instead of looping.
    #[test]
    fn route_review_refuses_a_senpai_cycle() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder] = seed_characters(&db, &["coder"])[..] else { unreachable!() };
        let a = senpai_ctx_row(&db, ws_id, "cycle-a", None, Some(coder));
        let b = senpai_ctx_row(&db, ws_id, "cycle-b", Some(&a), Some(coder));
        db.conn.execute(
            "UPDATE context_characters SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(b.context_id.as_bytes()), blob_param(a.context_id.as_bytes())],
        ).unwrap();
        let error = db.route_review(ReviewQuery::ask(a.context_id, coder)).unwrap_err();
        assert!(matches!(error, KernelDbError::Corrupt(_)), "a cycle is corruption, not a refusal: {error}");
        assert!(error.to_string().contains("cycle"), "{error}");
    }

    /// The senpai edge is the only route. `forked_from`, the director and the
    /// reviewer override on the row no longer decide anything.
    #[test]
    fn route_review_ignores_forked_from_and_the_old_routing_columns() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, judge, amy] = seed_characters(&db, &["coder", "banto", "judge", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "banto-seat", Some(&root), Some(banto));
        let judge_seat = senpai_ctx_row(&db, ws_id, "judge-seat", Some(&root), Some(judge));

        let mut lane = make_context_row(Some("routed-lane"));
        lane.forked_from = Some(seat.context_id);
        insert_context_with_doc(&db, &lane, ws_id);
        db.update_played_by(lane.context_id, Some(coder)).unwrap();
        db.set_context_senpai(lane.context_id, Some(judge_seat.context_id)).unwrap();

        assert_eq!(
            db.route_review(ReviewQuery::ask(lane.context_id, coder)).unwrap(),
            EffectiveReviewer::Assigned(judge),
            "the senpai's holder, not the fork parent's",
        );
    }

    /// A create routes before its context exists: `ReviewFrom::Senpai`
    /// starts at the senpai the new context will name.
    #[test]
    fn route_review_can_start_at_the_senpai_of_a_context_not_yet_written() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));

        assert_eq!(db.route_review(ReviewQuery::under(seat.context_id, coder)).unwrap(), EffectiveReviewer::Assigned(banto));
        assert_eq!(db.route_review(ReviewQuery::under(seat.context_id, banto)).unwrap(), EffectiveReviewer::Assigned(amy));
        assert_eq!(db.route_review(ReviewQuery::under(root.context_id, amy)).unwrap(), EffectiveReviewer::SelfConfirmation(amy));
        assert!(matches!(
            db.route_review(ReviewQuery::under(ContextId::new(), coder)).unwrap_err(),
            KernelDbError::NotFound(_)
        ));
    }

    /// Escalation runs the same climb past the current reviewer, one step at
    /// a time, and refuses at a context with no senpai, naming it.
    #[test]
    fn route_review_above_climbs_one_step_and_refuses_at_the_top() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto, amy] = seed_characters(&db, &["coder", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "escalate-root", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "escalate-seat", Some(&root), Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "escalate-lane", Some(&seat), Some(coder));

        let ask = ReviewQuery::ask(lane.context_id, coder);
        assert_eq!(db.route_review(ask).unwrap(), EffectiveReviewer::Assigned(banto));
        assert_eq!(
            db.route_review(ask.above(banto)).unwrap(),
            EffectiveReviewer::Assigned(amy),
            "escalation climbs one senpai further",
        );
        let error = db.route_review(ask.above(amy)).unwrap_err();
        assert!(matches!(error, KernelDbError::NoReviewerAbove(_)), "{error}");
        assert!(error.to_string().contains("escalate-root"), "names the top context: {error}");
        // Even a root actor does not confirm itself on the way up.
        let error = db.route_review(ReviewQuery::ask(root.context_id, amy).above(amy)).unwrap_err();
        assert!(matches!(error, KernelDbError::NoReviewerAbove(_)), "{error}");
    }

    /// The lineage root is the root character holding the top of a context's
    /// senpai chain. A chain that tops out anywhere else has no authority to
    /// change routing, and says so.
    #[test]
    fn lineage_root_requires_a_live_root_character_at_the_top() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [banto, coder, amy] = seed_characters(&db, &["banto", "coder", "amy"])[..] else { unreachable!() };

        let seat = senpai_ctx_row(&db, ws_id, "unrooted-seat", None, Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "unrooted-lane", Some(&seat), Some(coder));
        let error = db.lineage_root(lane.context_id).unwrap_err();
        assert!(error.to_string().contains("banto"), "names the character at the top: {error}");
        assert!(error.to_string().contains("not a root"), "{error}");

        let bare = senpai_ctx_row(&db, ws_id, "nobody-top", None, None);
        assert!(db.lineage_root(bare.context_id).unwrap_err().to_string().contains("no context character"));

        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        assert!(db.retire_character(amy, 1).unwrap());
        assert!(db.lineage_root(root.context_id).unwrap_err().to_string().contains("retired"));

        assert!(matches!(db.lineage_root(ContextId::new()).unwrap_err(), KernelDbError::NotFound(_)));
    }

    /// The lineage root follows the senpai chain, not `forked_from`.
    #[test]
    fn lineage_root_follows_senpai_not_forked_from() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [banto, coder, amy, bob] = seed_characters(&db, &["banto", "coder", "amy", "bob"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        db.update_character_root(bob, true).unwrap();
        let amy_root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let bob_root = senpai_ctx_row(&db, ws_id, "bob", None, Some(bob));
        let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&amy_root), Some(banto));
        let mut lane = make_context_row(Some("forked-from-amy-answers-to-bob"));
        lane.forked_from = Some(seat.context_id);
        insert_context_with_doc(&db, &lane, ws_id);
        db.update_played_by(lane.context_id, Some(coder)).unwrap();
        db.set_context_senpai(lane.context_id, Some(bob_root.context_id)).unwrap();

        assert_eq!(db.lineage_root(lane.context_id).unwrap(), bob);
        assert_eq!(db.lineage_top(lane.context_id).unwrap().context_id, bob_root.context_id);
        assert_eq!(db.lineage_root(seat.context_id).unwrap(), amy);
    }

    /// `lineage_top` names the top context itself and asks nothing of its
    /// character: a top held by a non-root still names a top.
    #[test]
    fn lineage_top_names_the_top_regardless_of_its_character() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [banto, coder] = seed_characters(&db, &["banto", "coder"])[..] else { unreachable!() };

        let seat = senpai_ctx_row(&db, ws_id, "unrooted-seat", None, Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "unrooted-lane", Some(&seat), Some(coder));
        assert!(db.lineage_root(lane.context_id).is_err());
        assert_eq!(db.lineage_top(lane.context_id).unwrap().context_id, seat.context_id);
        assert_eq!(db.lineage_top(seat.context_id).unwrap().context_id, seat.context_id, "a top names itself");
        assert!(matches!(db.lineage_top(ContextId::new()).unwrap_err(), KernelDbError::Validation(_)));
    }

    /// A senpai cycle is corruption; the lineage walk stops with a named
    /// error instead of looping, and so does the live variant.
    #[test]
    fn lineage_root_refuses_a_senpai_cycle() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [filler] = seed_characters(&db, &["filler"])[..] else { unreachable!() };
        let a = senpai_ctx_row(&db, ws_id, "cycle-a", None, Some(filler));
        let b = senpai_ctx_row(&db, ws_id, "cycle-b", Some(&a), Some(filler));
        db.conn.execute(
            "UPDATE context_characters SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(b.context_id.as_bytes()), blob_param(a.context_id.as_bytes())],
        ).unwrap();
        for error in [
            db.lineage_top(a.context_id).unwrap_err(),
            db.live_lineage_root(a.context_id).unwrap_err(),
        ] {
            assert!(matches!(error, KernelDbError::Validation(_)), "{error}");
            assert!(error.to_string().contains("cycle"), "{error}");
        }
    }

    /// `live_lineage_root` separates a chain with no live root (`None`) from a fault.
    #[test]
    fn live_lineage_root_is_none_without_a_root_and_an_error_on_a_fault() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [banto, amy] = seed_characters(&db, &["banto", "amy"])[..] else { unreachable!() };

        let bare = senpai_ctx_row(&db, ws_id, "nobody-top", None, None);
        assert_eq!(db.live_lineage_root(bare.context_id).unwrap(), None, "no holder");
        let seat = senpai_ctx_row(&db, ws_id, "not-root", None, Some(banto));
        assert_eq!(db.live_lineage_root(seat.context_id).unwrap(), None, "a non-root at the top");

        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        assert_eq!(db.live_lineage_root(root.context_id).unwrap(), Some(amy));
        assert!(db.retire_character(amy, 1).unwrap());
        assert_eq!(db.live_lineage_root(root.context_id).unwrap(), None, "a retired root");
        assert_eq!(db.live_lineage_root(ContextId::new()).unwrap(), None, "an ask may outlive its context");
    }

    /// A senpai pointer to a row that is not there is corruption, not a
    /// reason to stop climbing.
    #[test]
    fn lineage_top_refuses_a_dangling_senpai() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [filler] = seed_characters(&db, &["filler"])[..] else { unreachable!() };
        let lane = senpai_ctx_row(&db, ws_id, "dangling-lane", None, Some(filler));
        db.conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        db.conn.execute(
            "UPDATE context_characters SET senpai_ctx = ?1 WHERE context_id = ?2",
            params![blob_param(ContextId::new().as_bytes()), blob_param(lane.context_id.as_bytes())],
        ).unwrap();
        db.conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        let error = db.lineage_top(lane.context_id).unwrap_err();
        assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");
    }

    /// Moving a context under a new senpai refuses a self edge, a cycle, a
    /// missing senpai and a context with no context character; `None` makes
    /// it a top.
    #[test]
    fn set_context_senpai_moves_a_kohai_and_refuses_cycles_and_missing_rows() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [filler] = seed_characters(&db, &["filler"])[..] else { unreachable!() };
        let a = senpai_ctx_row(&db, ws_id, "mv-a", None, Some(filler));
        let b = senpai_ctx_row(&db, ws_id, "mv-b", Some(&a), Some(filler));
        let c = senpai_ctx_row(&db, ws_id, "mv-c", Some(&b), Some(filler));
        let d = senpai_ctx_row(&db, ws_id, "mv-d", None, Some(filler));
        let edge = |ctx: &ContextRow| db.senpai_of(ctx.context_id).unwrap();

        db.set_context_senpai(c.context_id, Some(d.context_id)).unwrap();
        assert_eq!(edge(&c), Some(d.context_id));
        assert_eq!(db.senpai_of(c.context_id).unwrap(), Some(d.context_id));
        let top = db.set_context_senpai(c.context_id, None).unwrap_err().to_string();
        assert!(top.contains("root console"), "only a root console has no senpai: {top}");
        assert_eq!(edge(&c), Some(d.context_id), "the refusal wrote nothing");
        db.set_context_senpai(c.context_id, Some(b.context_id)).unwrap();

        let own = db.set_context_senpai(a.context_id, Some(a.context_id)).unwrap_err().to_string();
        assert!(own.contains("mv-a") && own.contains("own senpai"), "{own}");
        let cycle = db.set_context_senpai(a.context_id, Some(c.context_id)).unwrap_err().to_string();
        assert!(cycle.contains("cycle") && cycle.contains("mv-a") && cycle.contains("mv-c"), "{cycle}");
        assert_eq!(edge(&a), None, "a refused move changes nothing");
        let missing = db.set_context_senpai(a.context_id, Some(ContextId::new())).unwrap_err();
        assert!(matches!(missing, KernelDbError::NotFound(_)), "{missing}");
        let no_row = db.set_context_senpai(ContextId::new(), Some(a.context_id)).unwrap_err();
        assert!(matches!(no_row, KernelDbError::NotFound(_)), "{no_row}");
        let bare = senpai_ctx_row(&db, ws_id, "mv-bare", None, None);
        let bare_error = db.set_context_senpai(bare.context_id, Some(a.context_id)).unwrap_err();
        assert!(bare_error.to_string().contains("no context character"), "{bare_error}");
    }

    /// A rotation moves no kohai: they keep pointing at the archived
    /// predecessor. It repoints a handoff log's pointer, and only that
    /// character's.
    #[test]
    fn commit_rotation_leaves_the_kohai_and_repoints_a_handoff_log() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [owner, other, kohai_character] = seed_characters(&db, &["owner", "other", "kohai"])[..] else { unreachable!() };
        let top = senpai_ctx_row(&db, ws_id, "rot-top", None, Some(owner));
        let old = senpai_ctx_row(&db, ws_id, "rot-old", Some(&top), Some(other));
        let kohai = senpai_ctx_row(&db, ws_id, "rot-kohai", Some(&old), Some(kohai_character));
        let new = senpai_ctx_row(&db, ws_id, "rot-new", None, None);
        db.write_context_character(new.context_id, other, Some(top.context_id), None).unwrap();

        let handoff = senpai_ctx_row(&db, ws_id, "rot-handoff", None, None);
        let handoff_next = senpai_ctx_row(&db, ws_id, "rot-handoff-next", None, None);
        db.set_character_handoff_ctx(owner, Some(handoff.context_id)).unwrap();

        assert_eq!(db.commit_rotation(old.context_id, new.context_id).unwrap(), 0, "no handoff log pointed at it");
        assert_eq!(db.senpai_of(kohai.context_id).unwrap(), Some(old.context_id), "the kohai still answer to the predecessor");
        assert!(db.list_kohai(new.context_id).unwrap().is_empty(), "the successor has none until it takes them");
        assert!(db.get_context(old.context_id).unwrap().unwrap().is_archived());

        assert_eq!(db.commit_rotation(handoff.context_id, handoff_next.context_id).unwrap(), 1);
        assert_eq!(db.get_character(owner).unwrap().unwrap().handoff_ctx, Some(handoff_next.context_id));
    }

    // ── 22d2. the one authority rule ────────────────────────────────────

    /// amy's console → banto's seat → two lanes. Over a lane, banto (who
    /// holds its senpai) and amy (its lineage root) have authority for every
    /// act the rule governs; the sibling lane's performer and the lane's own
    /// performer do not, and the refusal names the rule, the holder and the
    /// root.
    #[test]
    fn authority_rule_allows_the_senpai_holder_and_the_lineage_root_only() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, sibling, banto, amy] = seed_characters(&db, &["coder", "sibling", "banto", "amy"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        let root = senpai_ctx_row(&db, ws_id, "amy", None, Some(amy));
        let seat = senpai_ctx_row(&db, ws_id, "banto", Some(&root), Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&seat), Some(coder));
        let _other = senpai_ctx_row(&db, ws_id, "other-lane", Some(&seat), Some(sibling));

        for act in GovernedAct::ALL {
            assert_eq!(db.context_authority(lane.context_id, banto, act).unwrap(), Ok(Authority::SenpaiHolder), "{act:?}");
            assert_eq!(db.context_authority(lane.context_id, amy, act).unwrap(), Ok(Authority::LineageRoot), "{act:?}");
            for refused in [sibling, coder] {
                let why = db.context_authority(lane.context_id, refused, act).unwrap().unwrap_err();
                assert_eq!(why, format!(
                    "only lane ({})'s senpai (banto) or its lineage root (amy) may {}",
                    lane.context_id.short(), act.as_str()
                ));
            }
        }
        // Over the seat, amy holds the senpai and is also the root.
        assert_eq!(db.context_authority(seat.context_id, amy, GovernedAct::Move).unwrap(), Ok(Authority::SenpaiHolder));
        assert!(db.context_authority(seat.context_id, banto, GovernedAct::Move).unwrap().is_err(), "a seat has no authority over itself");
    }

    /// A context with no context character has no senpai and no lineage
    /// root, so a live root character may archive it; nobody else may, and
    /// no other act is opened.
    #[test]
    fn a_live_root_archives_a_context_with_no_context_character() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [amy, coder, old_root] = seed_characters(&db, &["amy", "coder", "old-root"])[..] else { unreachable!() };
        db.update_character_root(amy, true).unwrap();
        db.update_character_root(old_root, true).unwrap();
        db.retire_character(old_root, 1).unwrap();
        let orphan = senpai_ctx_row(&db, ws_id, "orphan", None, None);
        assert_eq!(db.context_authority(orphan.context_id, amy, GovernedAct::Archive).unwrap(), Ok(Authority::LiveRoot));
        for refused in [coder, old_root] {
            let why = db.context_authority(orphan.context_id, refused, GovernedAct::Archive).unwrap().unwrap_err();
            assert!(why.contains("has no context character") && why.contains("a live root character"), "{why}");
        }
        for act in GovernedAct::ALL.into_iter().filter(|act| *act != GovernedAct::Archive) {
            assert!(db.context_authority(orphan.context_id, amy, act).unwrap().is_err(), "{act:?}");
        }
    }

    /// A context with no senpai and a lineage with no live root still answer
    /// with the rule's text, naming what is missing; neither is a fault.
    #[test]
    fn authority_rule_names_a_missing_senpai_and_a_missing_root() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let [coder, banto] = seed_characters(&db, &["coder", "banto"])[..] else { unreachable!() };
        let top = senpai_ctx_row(&db, ws_id, "top", None, Some(banto));
        let lane = senpai_ctx_row(&db, ws_id, "lane", Some(&top), Some(coder));

        let why = db.context_authority(top.context_id, coder, GovernedAct::Archive).unwrap().unwrap_err();
        assert!(why.starts_with(&format!("only top ({})'s senpai (none) or its lineage root (none:", top.context_id.short())), "{why}");
        assert!(why.ends_with("may archive it"), "{why}");
        assert_eq!(db.context_authority(lane.context_id, banto, GovernedAct::Archive).unwrap(), Ok(Authority::SenpaiHolder),
            "holding the senpai is enough when the lineage has no root");
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

    /// `contexts_played_by` returns only the LIVE contexts a principal
    /// plays — an archived one drops out, matching what `kj character
    /// retire` must NOT try to archive twice.
    #[test]
    fn contexts_played_by_excludes_archived() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let character = PrincipalId::new();

        let live = make_context_row(Some("live-ctx"));
        insert_context_with_doc(&db, &live, ws_id);
        db.update_played_by(live.context_id, Some(character)).unwrap();

        let archived = make_context_row(Some("archived-ctx"));
        insert_context_with_doc(&db, &archived, ws_id);
        db.update_played_by(archived.context_id, Some(character)).unwrap();
        db.archive_context(archived.context_id).unwrap();

        let other = make_context_row(Some("other-ctx"));
        insert_context_with_doc(&db, &other, ws_id);
        db.update_played_by(other.context_id, Some(PrincipalId::new())).unwrap();

        let played = db.contexts_played_by(character).unwrap();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].context_id, live.context_id);
    }

    /// `name_for` checks the two sentinels first, then the sheet, and
    /// renders an unmapped principal as its short id rather than blank.
    #[test]
    fn name_for_resolves_sentinels_then_sheet_then_short_id() {
        let db = KernelDb::temporary().unwrap();
        assert_eq!(db.name_for(PrincipalId::system()), "system");
        assert_eq!(db.name_for(PrincipalId::beat()), "beat");

        let character = PrincipalId::new();
        db.insert_character(&CharacterRow { context_type: None,
            principal_id: character,
            name: "hajime".to_string(),
            created_at: 1000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
        assert_eq!(db.name_for(character), "hajime");

        let stranger = PrincipalId::new();
        assert_eq!(
            db.name_for(stranger),
            stranger.short(),
            "a credential with no characters row renders as its short id, never blank"
        );
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

    #[test]
    fn failed_well_known_publication_leaves_only_a_discardable_document() {
        for role in [WellKnownRole::DriftQueue, WellKnownRole::LostFound] {
            let db = KernelDb::temporary().unwrap();
            let workspace = setup_test_db(&db);
            let row = make_context_row(Some("unpublished"));
            db.insert_document(&DocumentRow {
                document_id: row.context_id, workspace_id: workspace,
                doc_kind: DocKind::Conversation, language: None, path: None,
                created_at: row.created_at, created_by: row.created_by,
            }).unwrap();
            db.conn.execute_batch("CREATE TRIGGER reject_role BEFORE INSERT ON well_known_contexts BEGIN SELECT RAISE(ABORT, 'injected role fault'); END;").unwrap();
            let error = db.insert_well_known_context(&row, workspace, role).unwrap_err();
            assert!(error.to_string().contains("injected role fault"), "{error}");
            assert!(db.get_context(row.context_id).unwrap().is_none());
            assert!(db.well_known_context(role).unwrap().is_none());
            assert!(db.get_document(row.context_id).unwrap().is_some());
            assert!(db.delete_document(row.context_id).unwrap());
            db.conn.execute_batch("DROP TRIGGER reject_role").unwrap();
            db.insert_well_known_context(&row, workspace, role).unwrap();
            assert_eq!(db.well_known_context(role).unwrap(), Some(row.context_id));
            assert!(db.get_context(row.context_id).unwrap().is_some());
        }
    }

    #[test]
    fn document_deletion_refuses_live_and_archived_contexts() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let ctx = make_context_row(Some("retained"));
        insert_context_with_doc(&db, &ctx, ws_id);
        for archived in [false, true] {
            if archived {
                db.archive_context(ctx.context_id).unwrap();
            }
            let error = db.delete_document(ctx.context_id).expect_err("context history must survive document deletion");
            assert!(error.to_string().contains("archive"), "{error}");
            assert!(db.get_document(ctx.context_id).unwrap().is_some());
            assert!(db.get_context(ctx.context_id).unwrap().is_some());
        }
    }

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

    #[test]
    fn promote_context_stamps_and_is_append_stable() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("candidate"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.promoted_at, None);

        assert_eq!(
            db.promote_context(cid).unwrap(),
            PromoteOutcome::Promoted,
            "first promote is new"
        );
        let loaded = db.get_context(cid).unwrap().unwrap();
        let stamp = loaded.promoted_at.expect("promoted_at set");
        assert!(stamp > 0);

        // Re-promote: no-op, original timestamp preserved (append-stable).
        assert_eq!(
            db.promote_context(cid).unwrap(),
            PromoteOutcome::AlreadyPromoted,
            "re-promote is a no-op"
        );
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.promoted_at, Some(stamp));
    }

    #[test]
    fn promote_clears_demoted_and_demote_clears_promoted() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("seesaw"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        // Promote → demoted_at stays clear.
        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert!(loaded.promoted_at.is_some());
        assert_eq!(loaded.demoted_at, None);

        // Demote a promoted context: one step in from ring 0 → unpromoted,
        // NOT straight to demoted (each demote call is one step outward).
        assert_eq!(
            db.demote_context(cid).unwrap(),
            DemoteOutcome::Unpromoted
        );
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.promoted_at, None);
        assert_eq!(loaded.demoted_at, None);

        // Demote again from neutral → demoted.
        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert!(loaded.demoted_at.is_some());

        // Promote a demoted context → clears demoted_at.
        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert!(loaded.promoted_at.is_some());
        assert_eq!(loaded.demoted_at, None);
    }

    #[test]
    fn demote_ladder_ends_archived() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("ladder"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);

        // promoted → unpromoted (back to automatic placement)
        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Unpromoted);
        // neither → demoted
        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
        // already demoted → archived (one step further outward)
        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Archived);

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert!(loaded.archived_at.is_some());

        // Further demote on an archived context is a loud error, not a
        // silent no-op — nothing further out to step to.
        let err = db.demote_context(cid).unwrap_err();
        assert!(matches!(err, KernelDbError::Validation(_)));
    }

    #[test]
    fn conclude_context_clears_promoted_seat() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("mux-exit"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
        assert!(db.conclude_context(cid).unwrap());

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.promoted_at, None, "conclude gives up the ring-0 seat");
    }

    #[test]
    fn archive_context_clears_promoted_and_demoted() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("archived-seat"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
        assert!(db.archive_context(cid).unwrap());

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.promoted_at, None);
        assert_eq!(loaded.demoted_at, None);
    }

    #[test]
    fn set_context_paused_roundtrip() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("pausable"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.paused_at, None);

        db.set_context_paused(cid, true).unwrap();
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert!(loaded.paused_at.is_some());

        db.set_context_paused(cid, false).unwrap();
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.paused_at, None);
    }

    #[test]
    fn active_ring_cap_refuses_an_eleventh_seat_until_a_demote() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let mut ids = Vec::new();
        for i in 0..ACTIVE_RING_CAPACITY {
            let row = make_context_row(Some(&format!("seat-{i}")));
            let cid = row.context_id;
            insert_context_with_doc(&db, &row, ws_id);
            assert_eq!(
                db.promote_context(cid).unwrap(),
                PromoteOutcome::Promoted,
                "seat {i} should promote"
            );
            ids.push(cid);
        }

        // 11th promote: ring full, fails loudly.
        let eleventh = make_context_row(Some("overflow"));
        let eleventh_id = eleventh.context_id;
        insert_context_with_doc(&db, &eleventh, ws_id);
        let err = db.promote_context(eleventh_id).unwrap_err();
        assert!(matches!(err, KernelDbError::Validation(_)), "err: {err:?}");

        // Demote one seat, freeing capacity for the 11th.
        assert_eq!(
            db.demote_context(ids[0]).unwrap(),
            DemoteOutcome::Unpromoted
        );
        assert_eq!(
            db.promote_context(eleventh_id).unwrap(),
            PromoteOutcome::Promoted,
            "11th promote should now succeed"
        );
    }

    #[test]
    fn promote_resurrects_an_archived_context() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("lazarus"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert!(db.archive_context(cid).unwrap());

        // Promote is the resurrection door: unarchives + seats in one act.
        assert_eq!(
            db.promote_context(cid).unwrap(),
            PromoteOutcome::Resurrected { concluded: false }
        );
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.archived_at, None, "resurrection clears archived_at");
        assert!(loaded.promoted_at.is_some(), "resurrection takes a seat");
        assert_eq!(loaded.demoted_at, None);

        // Back among the living: visible to list_active_contexts again.
        let active = db.list_active_contexts().unwrap();
        assert!(active.iter().any(|c| c.context_id == cid));
    }

    #[test]
    fn promote_resurrects_a_concluded_context_without_unconcluding() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("done-but-back"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert!(db.conclude_context(cid).unwrap());
        let concluded_stamp = db.get_context(cid).unwrap().unwrap().concluded_at.unwrap();
        assert!(db.archive_context(cid).unwrap());

        // Placement is orthogonal to conclusion: "concluded but seated for
        // review" is a legit state — resurrection must NOT clear concluded_at.
        assert_eq!(
            db.promote_context(cid).unwrap(),
            PromoteOutcome::Resurrected { concluded: true }
        );
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.archived_at, None);
        assert_eq!(loaded.concluded_at, Some(concluded_stamp));
        assert!(loaded.promoted_at.is_some());
    }

    #[test]
    fn ring_full_resurrection_errors_and_stays_archived() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        for i in 0..ACTIVE_RING_CAPACITY {
            let row = make_context_row(Some(&format!("full-seat-{i}")));
            let cid = row.context_id;
            insert_context_with_doc(&db, &row, ws_id);
            assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
        }

        let buried = make_context_row(Some("stays-buried"));
        let buried_id = buried.context_id;
        insert_context_with_doc(&db, &buried, ws_id);
        assert!(db.archive_context(buried_id).unwrap());

        // Resurrection needs a free seat exactly like any promote: full ring
        // → the same loud error, and NO partial write — the context stays
        // archived.
        let err = db.promote_context(buried_id).unwrap_err();
        assert!(matches!(err, KernelDbError::Validation(_)), "err: {err:?}");
        let loaded = db.get_context(buried_id).unwrap().unwrap();
        assert!(loaded.archived_at.is_some(), "failed resurrection must not unarchive");
        assert_eq!(loaded.promoted_at, None);
    }

    #[test]
    fn resurrection_stamps_a_fresh_promoted_at() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("stale-seat"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        // Defensive shape `archive_context` can't produce (it clears
        // promoted_at): an archived row still carrying an old seat stamp.
        // Resurrection must stamp FRESH — the old seat was surrendered at
        // archive time; a stale stamp would jump the append order.
        let stale_stamp: i64 = 12_345;
        db.conn
            .execute(
                "UPDATE contexts SET archived_at = ?1, promoted_at = ?2 WHERE context_id = ?3",
                params![99_999_i64, stale_stamp, blob_param(cid.as_bytes())],
            )
            .unwrap();

        assert_eq!(
            db.promote_context(cid).unwrap(),
            PromoteOutcome::Resurrected { concluded: false }
        );
        let loaded = db.get_context(cid).unwrap().unwrap();
        let fresh = loaded.promoted_at.expect("seated");
        assert!(
            fresh > stale_stamp,
            "resurrection must restamp promoted_at fresh (got {fresh}, stale was {stale_stamp})"
        );
    }

    #[test]
    fn resolve_context_full_id_reaches_an_archived_context() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("ghost"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        assert!(db.archive_context(cid).unwrap());

        // Fuzzy forms stay scoped to the active set — the label is gone…
        let err = db.resolve_context("ghost").unwrap_err();
        assert!(matches!(err, KernelDbError::NotFound(_)));

        // …but the exact full id is the resurrection handle.
        let hyphenated = cid.to_string();
        assert_eq!(db.resolve_context(&hyphenated).unwrap(), cid);
        let bare_hex = hyphenated.replace('-', "");
        assert_eq!(db.resolve_context(&bare_hex).unwrap(), cid);
    }

    /// `write_context` must persist every field the `ContextRow` carries —
    /// including the time-well placement stamps and `last_activity_at`. Today
    /// every insert caller passes `None` for all four, but an INSERT that
    /// omits the columns would silently drop values any future caller sets.
    #[test]
    fn insert_context_round_trips_placement_stamps() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let mut row = make_context_row(Some("stamped-at-birth"));
        row.last_activity_at = Some(1_111);
        row.promoted_at = Some(2_222);
        row.demoted_at = None; // promoted/demoted are mutually exclusive
        row.paused_at = Some(4_444);
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(loaded.last_activity_at, Some(1_111));
        assert_eq!(loaded.promoted_at, Some(2_222));
        assert_eq!(loaded.demoted_at, None);
        assert_eq!(loaded.paused_at, Some(4_444));
    }

    /// The ladder checks `demoted_at` FIRST, so a corrupt both-stamps row
    /// (a shape the kernel's mutual-exclusion writes can't produce) agrees
    /// with the app layout's demoted-wins rule: one demote archives it,
    /// instead of no-op-looping through unpromote while the stale demoted
    /// stamp keeps it visually in ring 3.
    #[test]
    fn demote_on_a_corrupt_both_stamps_row_archives() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("both-stamps"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        db.conn
            .execute(
                "UPDATE contexts SET promoted_at = ?1, demoted_at = ?2 WHERE context_id = ?3",
                params![1_000_i64, 2_000_i64, blob_param(cid.as_bytes())],
            )
            .unwrap();

        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Archived);
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert!(loaded.archived_at.is_some());
    }

    #[test]
    fn set_context_paused_rejects_archived_and_unknown() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("buried-napper"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);
        assert!(db.archive_context(cid).unwrap());

        // Archived: loud Validation, symmetric with demote's guard.
        let err = db.set_context_paused(cid, true).unwrap_err();
        assert!(matches!(err, KernelDbError::Validation(_)), "err: {err:?}");

        // Unknown: still NotFound, not conflated with the archived case.
        let err = db.set_context_paused(ContextId::new(), true).unwrap_err();
        assert!(matches!(err, KernelDbError::NotFound(_)), "err: {err:?}");
    }

    /// `paused_at` is orthogonal to ring placement and lifecycle: promote,
    /// demote, and conclude must all leave it alone.
    #[test]
    fn paused_at_survives_promote_demote_conclude() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("sleeper"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        db.set_context_paused(cid, true).unwrap();
        let stamp = db.get_context(cid).unwrap().unwrap().paused_at.unwrap();

        assert_eq!(db.promote_context(cid).unwrap(), PromoteOutcome::Promoted);
        assert_eq!(db.get_context(cid).unwrap().unwrap().paused_at, Some(stamp));

        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Unpromoted);
        assert_eq!(db.get_context(cid).unwrap().unwrap().paused_at, Some(stamp));

        assert!(db.conclude_context(cid).unwrap());
        assert_eq!(db.get_context(cid).unwrap().unwrap().paused_at, Some(stamp));
    }

    /// Conclude clears the ring-0 seat but deliberately preserves
    /// `demoted_at`: explicit demotion is sticky through conclusion, so a
    /// concluded-then-demoted (or demoted-then-concluded) context stays in
    /// the demoted ring instead of floating back to the auto pool.
    #[test]
    fn conclude_preserves_demoted_at() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("demoted-then-done"));
        let cid = row.context_id;
        insert_context_with_doc(&db, &row, ws_id);

        assert_eq!(db.demote_context(cid).unwrap(), DemoteOutcome::Demoted);
        let stamp = db.get_context(cid).unwrap().unwrap().demoted_at.unwrap();

        assert!(db.conclude_context(cid).unwrap());
        let loaded = db.get_context(cid).unwrap().unwrap();
        assert_eq!(
            loaded.demoted_at,
            Some(stamp),
            "conclude must not clear demoted_at — explicit demotion is sticky"
        );
    }

    /// The list SELECTs order by `COALESCE(last_activity_at, created_at)` —
    /// prove the recency competition actually reads `last_activity_at` when
    /// it's set (the other list-order tests always leave it `None`, so they
    /// only ever exercise the `created_at` fallback).
    #[test]
    fn list_orders_by_last_activity_over_created_at() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        // `old` was created first but touched most recently; `new` was
        // created later and never touched.
        let mut old = make_context_row(Some("old-but-hot"));
        old.created_at = 1_000;
        let mut new = make_context_row(Some("new-but-idle"));
        new.created_at = 2_000;
        insert_context_with_doc(&db, &old, ws_id);
        insert_context_with_doc(&db, &new, ws_id);
        db.touch_context_activity(old.context_id, 3_000).unwrap();

        let active = db.list_active_contexts().unwrap();
        let order: Vec<ContextId> = active.iter().map(|c| c.context_id).collect();
        assert_eq!(
            order,
            vec![new.context_id, old.context_id],
            "ascending COALESCE order: new (created 2000, untouched) before \
             old (touched 3000) — last_activity_at must beat created_at"
        );
    }

    /// Round-trips a block's provenance row and confirms it cascades away
    /// with its document.
    #[test]
    fn block_provenance_roundtrip_and_cascade() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
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

        let block_id = BlockId::new(doc_id, PrincipalId::system(), 7);
        let original = b"\x1b[31mred\x1b[0m plain";

        assert_eq!(db.get_block_provenance(&block_id, "ansi-strip").unwrap(), None);

        db.insert_block_provenance(&block_id, "ansi-strip", 1, original).unwrap();
        let (version, bytes) = db
            .get_block_provenance(&block_id, "ansi-strip")
            .unwrap()
            .expect("row just written");
        assert_eq!(version, 1);
        assert_eq!(bytes, original, "byte-exact round trip");

        // OR REPLACE: a re-run updates in place (version bumps, no dup row).
        db.insert_block_provenance(&block_id, "ansi-strip", 2, original).unwrap();
        assert_eq!(
            db.list_block_provenance(&block_id).unwrap(),
            vec![("ansi-strip".to_string(), 2, original.len())]
        );

        // Cascade with the document — the eviction story provenance rides on.
        db.conn
            .execute(
                "DELETE FROM documents WHERE document_id = ?1",
                params![blob_param(doc_id.as_bytes())],
            )
            .unwrap();
        assert_eq!(db.get_block_provenance(&block_id, "ansi-strip").unwrap(), None);
    }

    // ── 30. insert_document constraint classification, by read-back ─────
    //
    // `insert_document` failures used to be classified by matching
    // `e.to_string()` against "UNIQUE constraint" / "already exists", which
    // conflated the PRIMARY KEY conflict (same document, benign) with the
    // `idx_documents_path` conflict (a different document claiming a taken
    // path, divergent) and was only correct by luck of the message text.
    // These prove the typed, read-back-based classification.

    /// 1. Re-inserting a `DocumentRow` with an already-used `document_id`
    ///    must classify as `DuplicateDocument`, carrying the right id.
    #[test]
    fn insert_document_duplicate_id_is_typed_duplicate_document() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let doc_id = ContextId::new();
        let row = DocumentRow {
            document_id: doc_id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        };
        db.insert_document(&row).unwrap();

        // Same document_id again — same benign PRIMARY KEY conflict that
        // `BlockStore::create_document` recovers from.
        let err = db.insert_document(&row).unwrap_err();
        match err {
            KernelDbError::DuplicateDocument(id) => assert_eq!(id, doc_id),
            other => panic!("expected DuplicateDocument({doc_id}), got: {other}"),
        }
    }

    /// 2. Guard: `idx_documents_path` is a *partial* unique index
    ///    (`WHERE path IS NOT NULL`) — two documents with `path: None` in the
    ///    same workspace must both insert cleanly. If someone "fixes" the index
    ///    into a full unique index, this must fail loudly.
    #[test]
    fn insert_document_two_null_paths_in_same_workspace_both_succeed() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        for _ in 0..2 {
            db.insert_document(&DocumentRow {
                document_id: ContextId::new(),
                workspace_id: ws_id,
                doc_kind: DocKind::Conversation,
                language: None,
                path: None,
                created_at: now_millis(),
                created_by: PrincipalId::system(),
            })
            .unwrap();
        }
    }

    // 3. The existing FK-violation test (test 22, `fk_violation_is_validation_error`)
    // stays green — see that test above; not duplicated here.

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

    fn file_doc_row(id: ContextId, ws_id: WorkspaceId) -> DocumentRow {
        DocumentRow {
            document_id: id,
            workspace_id: ws_id,
            doc_kind: DocKind::File,
            language: Some("rust".to_string()),
            path: None,
            created_at: now_millis(),
            created_by: PrincipalId::system(),
        }
    }

    // ========================================================================
    // `context_state` and `archived_at` are written together. Before this,
    // archiving stamped only the timestamp, so every archived context read
    // back as `live` and the enum lied to anyone who trusted it alone.
    // ========================================================================

    #[test]
    fn archiving_sets_the_state_as_well_as_the_timestamp() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("to-archive"));
        insert_context_with_doc(&db, &row, ws_id);

        assert!(db.archive_context(row.context_id).unwrap());

        let loaded = db.get_context(row.context_id).unwrap().unwrap();
        assert_eq!(loaded.context_state, ContextState::Archived);
        assert!(loaded.archived_at.is_some());
        assert!(loaded.is_archived());
    }

    /// Resurrection derives the restored state from the timestamps that
    /// survived the archive, rather than remembering one.
    #[test]
    fn resurrecting_restores_live_or_concluded_from_the_timestamps() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);

        let plain = make_context_row(Some("plain"));
        insert_context_with_doc(&db, &plain, ws_id);
        db.archive_context(plain.context_id).unwrap();
        db.promote_context(plain.context_id).unwrap();
        let loaded = db.get_context(plain.context_id).unwrap().unwrap();
        assert_eq!(loaded.context_state, ContextState::Live);
        assert!(loaded.archived_at.is_none());
        assert!(!loaded.is_archived());

        let done = make_context_row(Some("done"));
        insert_context_with_doc(&db, &done, ws_id);
        db.conclude_context(done.context_id).unwrap();
        db.archive_context(done.context_id).unwrap();
        assert_eq!(
            db.get_context(done.context_id).unwrap().unwrap().context_state,
            ContextState::Archived,
            "archiving a concluded context reads as archived — the outer state wins"
        );
        db.promote_context(done.context_id).unwrap();
        let loaded = db.get_context(done.context_id).unwrap().unwrap();
        assert_eq!(
            loaded.context_state,
            ContextState::Concluded,
            "concluded_at survived the archive, so the restored state is concluded"
        );
    }

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

    #[test]
    fn insert_document_with_op_commits_the_row_and_the_op() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let id = ContextId::new();

        db.insert_document_with_op(&file_doc_row(id, ws_id), 1, b"payload")
            .unwrap();

        assert!(db.get_document(id).unwrap().is_some(), "the row must be committed");
        assert_eq!(
            db.load_oplog_since(id, 0).unwrap(),
            vec![(1i64, b"payload".to_vec())],
            "the first op must be committed with the row"
        );
    }

    /// The direction that matters: a failing op insert must take the document
    /// row down with it. Forced by planting an orphan oplog row (FKs off) at
    /// the seq the insert will use, so the op hits the oplog PRIMARY KEY.
    #[test]
    fn insert_document_with_op_rolls_the_row_back_when_the_op_fails() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let id = ContextId::new();

        db.conn.execute("PRAGMA foreign_keys = OFF", []).unwrap();
        db.conn
            .execute(
                "INSERT INTO oplog (document_id, seq, payload) VALUES (?1, ?2, ?3)",
                params![blob_param(id.as_bytes()), 1i64, b"squatter".as_slice()],
            )
            .unwrap();
        db.conn.execute("PRAGMA foreign_keys = ON", []).unwrap();

        let err = db
            .insert_document_with_op(&file_doc_row(id, ws_id), 1, b"payload")
            .unwrap_err();

        assert!(
            db.get_document(id).unwrap().is_none(),
            "the document row must roll back with the failed op, got: {err}"
        );
    }

    /// The other direction: a duplicate document id fails before the op is
    /// written, so no op lands for a row that was never created here.
    #[test]
    fn insert_document_with_op_writes_no_op_when_the_row_is_a_duplicate() {
        let mut db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let id = ContextId::new();

        db.insert_document_with_op(&file_doc_row(id, ws_id), 1, b"first")
            .unwrap();
        let err = db
            .insert_document_with_op(&file_doc_row(id, ws_id), 2, b"second")
            .unwrap_err();

        assert!(
            matches!(err, KernelDbError::DuplicateDocument(dup) if dup == id),
            "expected DuplicateDocument({id}), got: {err}"
        );
        assert_eq!(
            db.load_oplog_since(id, 0).unwrap(),
            vec![(1i64, b"first".to_vec())],
            "the second call must leave no op behind"
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
    fn an_archived_context_keeps_its_label_and_frees_it() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let old = make_context_row(Some("ROOT"));
        insert_context_with_doc(&db, &old, ws_id);
        assert!(db.archive_context(old.context_id).unwrap());

        let new = make_context_row(Some("ROOT"));
        insert_context_with_doc(&db, &new, ws_id);

        let archived = db.get_context(old.context_id).unwrap().unwrap();
        assert_eq!(archived.label.as_deref(), Some("ROOT"), "the archived row lost its name");
        let holder = db.find_context_by_label("ROOT").unwrap().expect("the live holder");
        assert_eq!(holder.context_id, new.context_id);
    }

    #[test]
    fn two_live_contexts_still_cannot_share_a_label() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        insert_context_with_doc(&db, &make_context_row(Some("ROOT")), ws_id);
        let dup = make_context_row(Some("ROOT"));
        let err = db
            .insert_context_with_document(&dup, ws_id)
            .expect_err("a second live ROOT must be refused");
        assert!(err.to_string().contains("already in use"), "{err}");
    }

    #[test]
    fn continuation_invalidation_migrates_without_closing_existing_work() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("legacy-continuation"));
        insert_context_with_doc(&db, &row, ws_id);
        let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
        db.conn.execute_batch("ALTER TABLE context_continuations DROP COLUMN invalidated_through").unwrap();
        KernelDb::apply_additive_migrations(&db.conn).unwrap();
        assert_eq!(db.continuation_epoch(row.context_id).unwrap(), Some(origin.epoch));
        assert!(db.admit_inference_request(row.context_id, origin.epoch, 1_100).unwrap());
        assert!(!db.admit_inference_request(row.context_id, origin.epoch + 1, 1_100).unwrap());
    }

    /// A context's character is fixed by its context character, so a second
    /// assignment of the same character keeps the continuation and another
    /// character is refused without touching it.
    #[test]
    fn a_performer_that_cannot_change_keeps_the_continuation() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("reassigned-continuation"));
        insert_context_with_doc(&db, &row, ws_id);
        let actor = PrincipalId::new();
        let other = PrincipalId::new();
        db.update_played_by(row.context_id, Some(actor)).unwrap();
        let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
        db.record_continuation_request(row.context_id, origin.epoch, 1_100).unwrap();
        db.record_continuation_yield(row.context_id, origin.epoch, 1_200).unwrap();
        db.update_played_by(row.context_id, Some(actor)).unwrap();
        assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());
        assert!(db.update_played_by(row.context_id, Some(other)).is_err());
        assert_eq!(db.get_context(row.context_id).unwrap().unwrap().played_by, Some(actor));
        assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());
    }

    #[test]
    fn continuation_window_requires_a_matching_yielded_unsigned_epoch() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("continuation"));
        insert_context_with_doc(&db, &row, ws_id);

        let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
        assert_eq!(origin.epoch, 1);
        assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 1_100, 600).unwrap());

        db.record_continuation_request(row.context_id, origin.epoch, 1_500).unwrap();
        db.record_continuation_yield(row.context_id, origin.epoch, 1_200).unwrap();
        assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 2_100, 600).unwrap());
        assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 2_101, 600).unwrap());

        db.sign_off_continuation(row.context_id, 1_300).unwrap();
        assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 1_400, 600).unwrap());

        let next = db.begin_continuation(row.context_id, 2_000).unwrap();
        assert_eq!(next.epoch, 2);
        db.record_continuation_request(row.context_id, next.epoch, 2_150).unwrap();
        db.record_continuation_yield(row.context_id, next.epoch, 2_100).unwrap();
        assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 2_200, 600).unwrap());
        assert!(db.automatic_resume_allowed(row.context_id, next.epoch, 2_200, 600).unwrap());
        assert!(db.claim_automatic_resume(row.context_id, next.epoch, 2_200, 600).unwrap());
        assert!(!db.claim_automatic_resume(row.context_id, next.epoch, 2_200, 600).unwrap());
        db.record_continuation_yield(row.context_id, next.epoch, 2_300).unwrap();
        db.sign_off_continuation(row.context_id, 2_400).unwrap();
        assert!(!db.claim_automatic_resume(row.context_id, next.epoch, 2_500, 600).unwrap());
    }

    #[test]
    fn interrupt_closes_a_yielded_continuation_independently_of_signoff() {
        let db = KernelDb::temporary().unwrap();
        let ws_id = setup_test_db(&db);
        let row = make_context_row(Some("interrupted"));
        insert_context_with_doc(&db, &row, ws_id);
        let interrupter = PrincipalId::new();

        // No continuation open yet: nothing to interrupt.
        assert!(!db.record_continuation_interrupt(row.context_id, interrupter, 900).unwrap());
        assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), None);

        let origin = db.begin_continuation(row.context_id, 1_000).unwrap();
        db.record_continuation_request(row.context_id, origin.epoch, 1_100).unwrap();
        db.record_continuation_yield(row.context_id, origin.epoch, 1_200).unwrap();
        assert!(db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());

        // Interrupt records who and when, and is idempotent.
        assert!(db.record_continuation_interrupt(row.context_id, interrupter, 1_250).unwrap());
        assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), Some((1_250, interrupter)));
        assert!(!db.record_continuation_interrupt(row.context_id, PrincipalId::new(), 1_260).unwrap(),
            "a second interrupt on an already-interrupted epoch records nothing new");
        assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), Some((1_250, interrupter)),
            "the second call must not overwrite who/when");

        // Interrupting closes automatic resume even though the epoch is
        // still yielded and within its window; signoff is untouched.
        assert!(!db.automatic_resume_allowed(row.context_id, origin.epoch, 1_300, 600).unwrap());
        assert!(!db.claim_automatic_resume(row.context_id, origin.epoch, 1_300, 600).unwrap());
        assert!(db.sign_off_continuation(row.context_id, 1_400).unwrap(),
            "interrupting a continuation does not set signed_off_at");

        // Opening a new epoch (an explicit `kj drive`) clears the interrupt.
        let next = db.begin_continuation(row.context_id, 2_000).unwrap();
        assert_eq!(db.continuation_interrupt(row.context_id).unwrap(), None);
        db.record_continuation_request(row.context_id, next.epoch, 2_100).unwrap();
        db.record_continuation_yield(row.context_id, next.epoch, 2_200).unwrap();
        assert!(db.automatic_resume_allowed(row.context_id, next.epoch, 2_300, 600).unwrap());
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

/// What ending a turn's hold did to its ask (`KernelDb::end_held_ask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeldAskEnd {
    /// Unanswered: abandoned.
    Abandoned,
    /// Answered but not run: spent without running.
    Spent,
    /// The approval worker already claimed it and settles the pair.
    Claimed,
}
