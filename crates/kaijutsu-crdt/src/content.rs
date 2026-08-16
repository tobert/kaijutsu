//! Per-block content with metadata.
//!
//! Each block owns its content as a plain `String`, plus a `BlockHeader` for
//! metadata (kind, role, status, parent_id, etc.). Text used to be a
//! per-block diamond-types-extended `Document` (a character-level CRDT); the
//! kernel is the sole sequencer for every mutation (CLAUDE.md "Durable state
//! and the wire"), so no block ever needed concurrent-branch reconciliation,
//! and the CRDT was retired in favor of the plain `String` it always
//! materialized down to. See `docs/crdt-position-2026-08.md`.

use crate::{BlockHeader, BlockId, BlockSnapshot, ContentType, PrincipalId, Status, TaskStatus};
use kaijutsu_types::Tick;

// Test-only instrumentation for `BlockContent::text()` — see
// `block_store::tests::statuses_ordered_does_not_materialize_block_text` for
// the regression this guards (a status-only accessor that quietly starts
// routing through `text()`/`snapshot()` again). Thread-local, not a global
// atomic: `cargo test` runs each `#[test]` fn on its own thread, so counts
// stay isolated per test without cross-test contamination or a `serial_test`
// dependency. Not compiled into non-test builds.
#[cfg(test)]
thread_local! {
    static TEXT_CALL_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_text_call_count() {
    TEXT_CALL_COUNT.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn text_call_count() -> usize {
    TEXT_CALL_COUNT.with(|c| c.get())
}

/// Value-based tiebreaker for per-field LWW merge.
///
/// Remote wins if its timestamp is higher, or if timestamps are equal and
/// the remote value is greater. Both peers compute the same result independently.
fn field_wins<T: Ord>(remote_ts: u64, local_ts: u64, remote_val: &T, local_val: &T) -> bool {
    remote_ts > local_ts || (remote_ts == local_ts && remote_val > local_val)
}

/// Base-62 charset for fractional indexing (0-9, A-Z, a-z).
/// Lexicographically ordered: '0' < '9' < 'A' < 'Z' < 'a' < 'z'.
pub const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Get the index of a character in the BASE62 charset.
fn base62_index(c: u8) -> usize {
    BASE62.iter().position(|&b| b == c).unwrap_or(0)
}

/// Encode a non-negative integer as a fixed-width, '0'-left-padded base-62
/// string. Fixed width makes lexicographic order match numeric order, so a
/// monotonic counter yields monotonically-sorting keys of bounded length —
/// the basis for tick-derived `order_key`s that append in O(1) without growth.
/// Width 11 covers the full `i64` range (62^11 > i64::MAX).
pub fn base62_encode_padded(value: i64, width: usize) -> String {
    let mut n = value.max(0) as u64;
    let mut buf = vec![b'0'; width];
    let mut i = width;
    while n > 0 && i > 0 {
        i -= 1;
        buf[i] = BASE62[(n % 62) as usize];
        n /= 62;
    }
    String::from_utf8(buf).expect("base62 chars are valid utf8")
}

/// Compute a lexicographic midpoint between two base-62 strings.
///
/// Empty string `""` sorts before everything. Both `a` and `b` must satisfy `a < b`
/// lexicographically. The result is guaranteed to satisfy `a < result < b`.
pub(crate) fn order_midpoint(a: &str, b: &str) -> String {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let max_len = a_bytes.len().max(b_bytes.len());

    let mut result = Vec::new();

    for i in 0..=max_len {
        let a_val = if i < a_bytes.len() {
            base62_index(a_bytes[i])
        } else {
            0
        };
        let b_val = if i < b_bytes.len() {
            base62_index(b_bytes[i])
        } else {
            62
        };

        if a_val + 1 < b_val {
            let mid = (a_val + b_val) / 2;
            result.push(BASE62[mid]);
            return String::from_utf8(result).unwrap_or_else(|_| "V".to_string());
        } else if a_val == b_val {
            result.push(BASE62[a_val]);
        } else {
            result.push(BASE62[a_val]);
            let a_next = if i + 1 < a_bytes.len() {
                base62_index(a_bytes[i + 1])
            } else {
                0
            };
            let mid = (a_next + 62) / 2;
            result.push(BASE62[mid]);
            return String::from_utf8(result).unwrap_or_else(|_| "V".to_string());
        }
    }

    result.push(BASE62[31]); // 'V'
    String::from_utf8(result).unwrap_or_else(|_| "V".to_string())
}

/// Decode a fixed-width base-62 string to i64. `None` if the string is empty,
/// any char is not in the BASE62 charset, or the accumulated value overflows
/// i64.
///
/// Empty rejection is deliberate: a zero-length string has no digits, so it is
/// not a valid canonical key. Returning `None` (not `Some(0)`) keeps a bogus
/// zero-length pred from feeding `order_key_successor` a `V00000000001…`
/// successor — fail-loud over a silently-wrong value.
///
/// The inverse of [`base62_encode_padded`] for the canonical width-11 keys —
/// leading '0' padding decodes back to the same number, so the round-trip is
/// lossless for any non-negative value that fit the chosen width.
pub(crate) fn base62_decode(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &b in s.as_bytes() {
        let digit = BASE62.iter().position(|&c| c == b)? as i64;
        n = n.checked_mul(62)?.checked_add(digit)?;
    }
    Some(n)
}

/// The canonical append key strictly after `pred`, carrying `suffix` (the
/// inserting agent's 4-char lane).
///
/// Ordering NEVER derives from a counter on appends — only from the
/// predecessor's key — so a stale tick/seq counter structurally cannot
/// mis-sort an appended block (design §2.1).
///
/// Canonical fast path: `pred` is 'V' + 11 base-62 chars (the
/// `order_key_for_tick` shape; any trailing suffix chars are ignored) → decode
/// the 11-char number n, checked `n + 1`, re-encode at width 11 →
/// `"V{n+1}{suffix}"`. O(1) and shape-identical to every existing canonical key.
///
/// General fallback (legacy/variable-length pred: `order_midpoint` results,
/// `"{after}V"` appends, `"{:020}"` decimal restore keys — which sort below 'V'
/// since '0' < 'V' — or `n + 1` overflow at `i64::MAX`): `"{pred}V{suffix}"`,
/// strictly greater than `pred` by prefix extension. Emits `tracing::warn`
/// (this path grows keys and should be rare); never wraps.
pub(crate) fn order_key_successor(pred: &str, suffix: &str) -> String {
    // Canonical shape: 'V' + at least 11 base-62 chars. The first 11 after 'V'
    // are the width-11 number; anything past that is the agent suffix and is
    // ignored for the increment. `get(1..12)` returns None if pred is shorter
    // than 12 bytes OR 12 isn't a char boundary (a non-ASCII legacy key) — both
    // fall through to the prefix-extension fallback rather than panicking on a
    // byte slice.
    let number = if pred.as_bytes().first() == Some(&b'V') {
        pred.get(1..12)
    } else {
        None
    };
    if let Some(number) = number
        && let Some(n) = base62_decode(number)
        && let Some(next) = n.checked_add(1)
    {
        let key = format!("V{}{}", base62_encode_padded(next, 11), suffix);
        // Belt and braces: the increment can only sort after pred. If it
        // somehow didn't, fall through to the prefix-extension fallback
        // rather than emit a non-monotonic key.
        if key.as_str() > pred {
            return key;
        }
        tracing::error!(
            pred = %pred,
            candidate = %key,
            "order_key_successor: canonical increment not > pred, using fallback"
        );
    }

    // General fallback: strictly greater than pred by prefix extension. Grows
    // the key by 5 chars; rare by construction once appends are successor-derived.
    tracing::warn!(
        pred = %pred,
        "order_key_successor: non-canonical predecessor, using prefix-extension fallback"
    );
    format!("{pred}V{suffix}")
}

/// A single block's content and metadata.
///
/// `text` holds the block's content directly — no per-block CRDT, because no
/// block is ever concurrently edited (the kernel is the sole sequencer for
/// every mutation; see CLAUDE.md "Durable state and the wire"). The
/// BlockHeader holds identity, kind, role, status, etc. (plain data). The
/// order_key determines sibling ordering (fractional index).
pub struct BlockContent {
    /// Block identity + metadata.
    header: BlockHeader,

    /// This block's content, char-indexed (never byte-indexed — the project
    /// has already been bitten by that confusion once; see `edit_text`).
    /// Text blocks: the message text. ToolCall blocks: tool_input
    /// (streamable JSON). File blocks: the full file content.
    text: String,

    /// `text.chars().count()`, maintained incrementally on every edit rather
    /// than recomputed. The kernel reads this once per streamed token (to
    /// stamp the journaled `TextEdit`'s `pos`); a `chars().count()` rescan
    /// there would make every token O(the whole block's length so far)
    /// instead of O(the token's own length) — see `append_text`.
    char_len: usize,

    /// Fractional index for sibling ordering (base-62 lexicographic).
    /// Calculated via order_midpoint() on insertion (or derived from `tick`).
    order_key: String,

    /// Semantic timeline coordinate (hyoushigi). `Some` for blocks created on a
    /// timeline (every freshly-inserted block); the `order_key` is derived to
    /// match tick order on append. Distinct axis from `order_key` — see
    /// `docs/hyoushigi.md`.
    tick: Option<Tick>,

    /// Stable lane identity (hyoushigi `TrackId`). `Some` ONLY on a block
    /// materialized from a committed timeline cell — write-once metadata, like
    /// the snapshot fields below. LANE IDENTITY ONLY: the lane key is never the
    /// author (the author stays `BlockId.principal_id`); `track == None` matches
    /// no track.
    track: Option<kaijutsu_types::TrackId>,

    /// Non-Copy snapshot fields that don't belong on BlockHeader.
    /// These are write-once metadata set at creation time.
    tool_name: Option<String>,
    tool_input: Option<String>,
    tool_call_id: Option<BlockId>,
    tool_use_id: Option<String>,
    output: Option<kaijutsu_types::OutputData>,
    /// Standard error stream, persisted separately from `content` (stdout).
    /// Set once at tool completion via [`set_stderr`](Self::set_stderr).
    stderr: Option<String>,
    /// Reasoning-continuity token for Thinking blocks. Set once at
    /// `ThinkingEnd` via [`set_signature`](Self::set_signature). See
    /// [`kaijutsu_types::BlockSnapshot::signature`].
    signature: Option<String>,
    source_context: Option<crate::ContextId>,
    source_model: Option<String>,
    drift_kind: Option<crate::DriftKind>,
    file_path: Option<String>,

    /// Whether this block is collapsed (only meaningful for Thinking blocks).
    collapsed: bool,

    /// Ephemeral blocks are displayed but excluded from LLM hydration.
    ephemeral: bool,

    /// User-curated exclusion — toggled during staging.
    excluded: bool,

    /// Structured error payload (for error blocks).
    error: Option<kaijutsu_types::ErrorPayload>,

    /// Structured notification payload (for notification blocks).
    notification: Option<kaijutsu_types::NotificationPayload>,

    /// Structured resource payload (for resource blocks).
    resource: Option<kaijutsu_types::ResourcePayload>,

    /// Whether this block has been deleted (tombstone).
    deleted: bool,
}

impl BlockContent {
    /// Create a new block with empty content.
    ///
    /// `_principal_id` is unused now that content has no per-replica CRDT
    /// agent — kept as a parameter so every call site across the kernel
    /// (which threads authorship through it) does not need to change.
    pub fn new(header: BlockHeader, _principal_id: PrincipalId, order_key: String) -> Self {
        Self {
            header,
            text: String::new(),
            char_len: 0,
            order_key,
            tick: None,
            track: None,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            tool_use_id: None,
            output: None,
            stderr: None,
            signature: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
            file_path: None,
            error: None,
            notification: None,
            resource: None,
            collapsed: false,
            ephemeral: false,
            excluded: false,
            deleted: false,
        }
    }

    /// Create a new block with initial text content. `tick` is the block's
    /// timeline coordinate (`None` only when restoring a pre-tick block).
    pub fn with_content(
        header: BlockHeader,
        content: &str,
        principal_id: PrincipalId,
        order_key: String,
        tick: Option<Tick>,
    ) -> Self {
        let mut block = Self::new(header, principal_id, order_key);
        block.tick = tick;
        block.char_len = content.chars().count();
        block.text = content.to_string();
        block
    }

    /// Create from a full BlockSnapshot (for restoring from persistence).
    ///
    /// If the snapshot carries an `order_key`, it is used; otherwise falls back
    /// to `fallback_order_key`.
    pub fn from_snapshot(
        snap: &BlockSnapshot,
        principal_id: PrincipalId,
        fallback_order_key: String,
    ) -> Self {
        let header = BlockHeader::from_snapshot(snap);
        let order_key = snap.order_key.clone().unwrap_or(fallback_order_key);
        let mut block = Self::with_content(header, &snap.content, principal_id, order_key, snap.tick);
        // Lane identity rides through restore with the block (write-once); the
        // author stays `BlockId.principal_id`, never derived from track.
        block.track = snap.track.clone();
        block.tool_name = snap.tool_name.clone();
        block.tool_input = snap.tool_input.clone();
        block.tool_call_id = snap.tool_call_id;
        block.tool_use_id = snap.tool_use_id.clone();
        block.output = snap.output.clone();
        block.stderr = snap.stderr.clone();
        block.signature = snap.signature.clone();
        block.source_context = snap.source_context;
        block.source_model = snap.source_model.clone();
        block.drift_kind = snap.drift_kind;
        block.file_path = snap.file_path.clone();
        block.error = snap.error.clone();
        block.notification = snap.notification.clone();
        block.resource = snap.resource.clone();
        block.collapsed = snap.collapsed;
        block.ephemeral = snap.ephemeral;
        block.excluded = snap.excluded;
        block
    }

    // ── Content access ──────────────────────────────────────────────────

    /// Get the current text content.
    pub fn text(&self) -> String {
        #[cfg(test)]
        TEXT_CALL_COUNT.with(|c| c.set(c.get() + 1));
        self.text.clone()
    }

    /// Convert a CHARACTER index into the corresponding byte index into
    /// `s`. `char_idx == s.chars().count()` (one past the last char, the
    /// append position) maps to `s.len()`. Never byte-indexes directly —
    /// callers pass character offsets, and this project has already been
    /// bitten by the byte/char confusion once (see `kaijutsu:read`/`edit`'s
    /// hashline addressing).
    fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(byte_idx, _)| byte_idx)
            .unwrap_or(s.len())
    }

    /// Edit text at a position (insert and/or delete). `pos` and `delete`
    /// are CHARACTER offsets, never byte offsets.
    pub fn edit_text(&mut self, pos: usize, insert: &str, delete: usize) {
        if delete == 0 && insert.is_empty() {
            return;
        }
        let byte_start = Self::char_to_byte(&self.text, pos);
        let byte_end = Self::char_to_byte(&self.text, pos + delete);
        self.text.replace_range(byte_start..byte_end, insert);
        self.char_len = self.char_len - delete + insert.chars().count();
    }

    /// Append text to the end. `String::push_str` is amortized O(1) and
    /// `char_len`'s update is O(the token's own length), never O(the whole
    /// block) — no re-materialization, since this runs once per streamed
    /// token.
    pub fn append_text(&mut self, text: &str) {
        self.text.push_str(text);
        self.char_len += text.chars().count();
    }

    /// Get the character count of the content. O(1) — reads the counter
    /// `edit_text`/`append_text` maintain incrementally, not a rescan.
    pub fn content_len(&self) -> usize {
        self.char_len
    }

    // ── Metadata access ─────────────────────────────────────────────────

    /// Get the block ID.
    pub fn id(&self) -> BlockId {
        self.header.id
    }

    /// Get an immutable reference to the header.
    pub fn header(&self) -> &BlockHeader {
        &self.header
    }

    /// Get the ordering key.
    pub fn order_key(&self) -> &str {
        &self.order_key
    }

    /// Set the ordering key (for move operations).
    pub fn set_order_key(&mut self, key: String) {
        self.order_key = key;
    }

    /// The block's timeline coordinate, if it has one.
    pub fn tick(&self) -> Option<Tick> {
        self.tick
    }

    /// Set the timeline coordinate (used at insert and during normalization).
    pub fn set_tick(&mut self, tick: Tick) {
        self.tick = Some(tick);
    }

    /// Set the status, bumping per-field and aggregate timestamps.
    pub fn set_status(&mut self, status: Status, lamport_ts: u64) {
        self.header.status = status;
        self.header.status_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    /// Set collapsed state, bumping per-field and aggregate timestamps.
    pub fn set_collapsed(&mut self, collapsed: bool, lamport_ts: u64) {
        self.collapsed = collapsed;
        self.header.collapsed = collapsed;
        self.header.collapsed_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    /// Whether this block is deleted (tombstone).
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// Mark as deleted (tombstone).
    pub fn mark_deleted(&mut self, lamport_ts: u64) {
        self.deleted = true;
        self.header.updated_at = lamport_ts;
    }

    // ── Snapshot fields ─────────────────────────────────────────────────

    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    pub fn set_tool_name(&mut self, name: Option<String>) {
        self.tool_name = name;
    }

    pub fn tool_input(&self) -> Option<&str> {
        self.tool_input.as_deref()
    }

    pub fn set_tool_input(&mut self, input: Option<String>) {
        self.tool_input = input;
    }

    pub fn tool_call_id(&self) -> Option<BlockId> {
        self.tool_call_id
    }

    pub fn set_tool_call_id(&mut self, id: Option<BlockId>) {
        self.tool_call_id = id;
    }

    pub fn output(&self) -> Option<&kaijutsu_types::OutputData> {
        self.output.as_ref()
    }

    pub fn set_output(&mut self, output: Option<kaijutsu_types::OutputData>) {
        self.output = output;
    }

    pub fn stderr(&self) -> Option<&str> {
        self.stderr.as_deref()
    }

    /// Set the standard-error stream (write-once at tool completion).
    pub fn set_stderr(&mut self, stderr: Option<String>) {
        self.stderr = stderr;
    }

    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    /// Set the reasoning-continuity token (write-once at `ThinkingEnd`).
    pub fn set_signature(&mut self, signature: Option<String>) {
        self.signature = signature;
    }

    pub fn source_context(&self) -> Option<crate::ContextId> {
        self.source_context
    }

    pub fn source_model(&self) -> Option<&str> {
        self.source_model.as_deref()
    }

    pub fn drift_kind(&self) -> Option<crate::DriftKind> {
        self.drift_kind
    }

    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    pub fn set_file_path(&mut self, path: Option<String>) {
        self.file_path = path;
    }

    pub fn tool_use_id(&self) -> Option<&str> {
        self.tool_use_id.as_deref()
    }

    pub fn set_tool_use_id(&mut self, id: Option<String>) {
        self.tool_use_id = id;
    }

    pub fn content_type(&self) -> ContentType {
        self.header.content_type
    }

    pub fn set_content_type(&mut self, ct: ContentType, lamport_ts: u64) {
        self.header.content_type = ct;
        self.header.content_type_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    /// Get the task lifecycle status (`BlockKind::Task` only — meaningless
    /// on other kinds).
    pub fn task_status(&self) -> TaskStatus {
        self.header.task_status
    }

    /// Set the task lifecycle status, bumping per-field and aggregate
    /// timestamps. Mirrors `set_content_type` exactly — same "Copy field on
    /// `BlockHeader` with its own LWW clock" mechanism, giving task status
    /// the same multi-frontend CRDT sync for free.
    pub fn set_task_status(&mut self, status: TaskStatus, lamport_ts: u64) {
        self.header.task_status = status;
        self.header.task_status_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    pub fn ephemeral(&self) -> bool {
        self.ephemeral
    }

    pub fn set_ephemeral(&mut self, val: bool, lamport_ts: u64) {
        self.ephemeral = val;
        self.header.ephemeral = val;
        self.header.ephemeral_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    pub fn excluded(&self) -> bool {
        self.excluded
    }

    pub fn set_excluded(&mut self, val: bool, lamport_ts: u64) {
        self.excluded = val;
        self.header.excluded = val;
        self.header.excluded_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    /// Update `exit_code` and bump `tool_meta_at` (the tool_meta cluster's
    /// LWW timestamp covering `tool_kind`, `exit_code`, `is_error`).
    pub fn set_exit_code(&mut self, val: Option<i32>, lamport_ts: u64) {
        self.header.exit_code = val;
        self.header.tool_meta_at = lamport_ts;
        self.header.updated_at = self.header.max_field_ts();
    }

    // ── Snapshot ─────────────────────────────────────────────────────────

    /// Freeze this block into a BlockSnapshot.
    pub fn snapshot(&self) -> BlockSnapshot {
        BlockSnapshot {
            id: self.header.id,
            parent_id: self.header.parent_id,
            role: self.header.role,
            status: self.header.status,
            kind: self.header.kind,
            content: self.text(),
            collapsed: self.collapsed,
            ephemeral: self.ephemeral,
            excluded: self.excluded,
            created_at: self.header.created_at,
            tool_kind: self.header.tool_kind,
            tool_name: self.tool_name.clone(),
            tool_input: self.tool_input.clone(),
            tool_use_id: self.tool_use_id.clone(),
            tool_call_id: self.tool_call_id,
            exit_code: self.header.exit_code,
            is_error: self.header.is_error,
            stderr: self.stderr.clone(),
            signature: self.signature.clone(),
            output: self.output.clone(),
            source_context: self.source_context,
            source_model: self.source_model.clone(),
            drift_kind: self.drift_kind,
            file_path: self.file_path.clone(),
            error: self.error.clone(),
            notification: self.notification.clone(),
            resource: self.resource.clone(),
            task_status: self.header.task_status,
            content_type: self.header.content_type,
            order_key: Some(self.order_key.clone()),
            tick: self.tick,
            track: self.track.clone(),
            updated_at: self.header.updated_at,
            status_at: self.header.status_at,
            collapsed_at: self.header.collapsed_at,
            ephemeral_at: self.header.ephemeral_at,
            excluded_at: self.header.excluded_at,
            tool_meta_at: self.header.tool_meta_at,
            content_type_at: self.header.content_type_at,
            task_status_at: self.header.task_status_at,
        }
    }

    /// Merge a remote header using per-field LWW.
    ///
    /// Each mutable field group has its own Lamport timestamp. When timestamps
    /// tie, the greater value wins (value-based tiebreaker). Both peers
    /// independently compute the same result, guaranteeing convergence.
    pub fn merge_header(&mut self, remote: &BlockHeader) {
        // status
        if field_wins(
            remote.status_at,
            self.header.status_at,
            &remote.status,
            &self.header.status,
        ) {
            self.header.status = remote.status;
            self.header.status_at = remote.status_at;
        }

        // collapsed
        if field_wins(
            remote.collapsed_at,
            self.header.collapsed_at,
            &remote.collapsed,
            &self.header.collapsed,
        ) {
            self.header.collapsed = remote.collapsed;
            self.collapsed = remote.collapsed;
            self.header.collapsed_at = remote.collapsed_at;
        }

        // ephemeral
        if field_wins(
            remote.ephemeral_at,
            self.header.ephemeral_at,
            &remote.ephemeral,
            &self.header.ephemeral,
        ) {
            self.header.ephemeral = remote.ephemeral;
            self.ephemeral = remote.ephemeral;
            self.header.ephemeral_at = remote.ephemeral_at;
        }

        // excluded
        if field_wins(
            remote.excluded_at,
            self.header.excluded_at,
            &remote.excluded,
            &self.header.excluded,
        ) {
            self.header.excluded = remote.excluded;
            self.excluded = remote.excluded;
            self.header.excluded_at = remote.excluded_at;
        }

        // tool_meta (tool_kind, exit_code, is_error)
        if field_wins(
            remote.tool_meta_at,
            self.header.tool_meta_at,
            &remote.tool_kind,
            &self.header.tool_kind,
        ) {
            self.header.tool_kind = remote.tool_kind;
            self.header.exit_code = remote.exit_code;
            self.header.is_error = remote.is_error;
            self.header.tool_meta_at = remote.tool_meta_at;
        }

        // content_type
        if field_wins(
            remote.content_type_at,
            self.header.content_type_at,
            &remote.content_type,
            &self.header.content_type,
        ) {
            self.header.content_type = remote.content_type;
            self.header.content_type_at = remote.content_type_at;
        }

        // task_status
        if field_wins(
            remote.task_status_at,
            self.header.task_status_at,
            &remote.task_status,
            &self.header.task_status,
        ) {
            self.header.task_status = remote.task_status;
            self.header.task_status_at = remote.task_status_at;
        }

        // Recompute aggregate timestamp
        self.header.updated_at = self.header.max_field_ts().max(remote.max_field_ts());
    }
}

#[cfg(test)]
mod successor_tests {
    use super::*;

    // T1: the order_key_successor unit suite. Pins the append-key primitive
    // that decouples ordering from the tick counter (design §2.1).

    #[test]
    fn base62_decode_round_trips_canonical_width() {
        for n in [0i64, 1, 61, 62, 1234567, i64::MAX] {
            let enc = base62_encode_padded(n, 11);
            assert_eq!(base62_decode(&enc), Some(n), "round-trip {n}");
        }
    }

    #[test]
    fn base62_decode_rejects_non_base62_and_empty() {
        // '+' is not in the base62 charset.
        assert_eq!(base62_decode("12+34"), None);
        // The empty string is NOT a valid canonical key — it has no digits, so
        // there is no number to decode. Returning None (rather than Some(0))
        // forecloses the zero-prefix footgun: a zero-length pred can never feed a
        // bogus `V00000000001…` successor. Fail-loud over a silently-wrong value.
        assert_eq!(base62_decode(""), None);
    }

    #[test]
    fn base62_decode_overflow_is_none() {
        // 12 base62 'z' chars exceed i64::MAX.
        assert_eq!(base62_decode("zzzzzzzzzzzz"), None);
    }

    #[test]
    fn canonical_fast_path_increments_and_carries_suffix() {
        // V + 11-base62 number + 4-char suffix is the order_key_for_tick shape.
        let pred = format!("V{}{}", base62_encode_padded(41, 11), "AAAA");
        let succ = order_key_successor(&pred, "BBBB");
        // n+1 == 42, re-encoded at width 11, with our own suffix.
        let expected = format!("V{}{}", base62_encode_padded(42, 11), "BBBB");
        assert_eq!(succ, expected);
        assert!(succ > pred, "successor must sort strictly after pred");
    }

    #[test]
    fn canonical_fast_path_ignores_trailing_suffix_chars() {
        // Trailing suffix chars on pred are ignored; only the 11-char number counts.
        let pred = format!("V{}{}", base62_encode_padded(7, 11), "zzzz");
        let succ = order_key_successor(&pred, "0000");
        let expected = format!("V{}{}", base62_encode_padded(8, 11), "0000");
        assert_eq!(succ, expected);
    }

    #[test]
    fn legacy_midpoint_shape_uses_prefix_extension_fallback() {
        // order_midpoint produces variable-length non-canonical keys.
        let pred = order_midpoint("V", "Va");
        let succ = order_key_successor(&pred, "XXXX");
        assert_eq!(succ, format!("{pred}V{}", "XXXX"));
        assert!(succ > pred);
    }

    #[test]
    fn legacy_after_v_append_shape_uses_fallback() {
        // The "{after}V" append shape is not canonical (no leading 'V'+11 number
        // unless after itself was canonical — here it starts with a digit).
        let pred = "0042V";
        let succ = order_key_successor(pred, "QQQQ");
        assert_eq!(succ, format!("{pred}V{}", "QQQQ"));
        assert!(succ.as_str() > pred);
    }

    #[test]
    fn legacy_decimal_restore_key_uses_fallback_and_sorts_after() {
        // "{:020}" decimal restore keys sort below 'V' ('0' < 'V'); successor must
        // still be strictly greater than the decimal pred itself.
        let pred = format!("{:020}", 5);
        let succ = order_key_successor(&pred, "MMMM");
        assert_eq!(succ, format!("{pred}V{}", "MMMM"));
        assert!(succ > pred);
    }

    #[test]
    fn i64_max_canonical_falls_back_no_wrap() {
        // n+1 overflows at i64::MAX → general fallback, never wraps to a small key.
        let pred = format!("V{}{}", base62_encode_padded(i64::MAX, 11), "AAAA");
        let succ = order_key_successor(&pred, "BBBB");
        assert_eq!(succ, format!("{pred}V{}", "BBBB"));
        assert!(succ > pred, "must not wrap below pred");
    }

    #[test]
    fn multibyte_predecessor_falls_back_without_panic() {
        // A non-ASCII pred starting with 'V' must NOT panic on the `[1..12]` byte
        // slice (12 may land mid-codepoint) — it falls through to the prefix
        // extension. Order keys are ASCII by construction, so this is belt-and-
        // braces against a malformed/legacy key, never a live path.
        // 'V' + 9 ASCII (bytes 1..=9) + a 4-byte emoji at bytes 10..=13, so byte 12
        // lands mid-codepoint — `&pred[1..12]` would have panicked; `get(1..12)`
        // returns None and we fall through.
        let pred = "V012345678🎷";
        assert!(!pred.is_char_boundary(12), "byte 12 must split the emoji for this pin");
        let succ = order_key_successor(pred, "BBBB");
        assert_eq!(succ, format!("{pred}V{}", "BBBB"));
        assert!(succ.as_str() > pred, "fallback still sorts strictly after pred");
    }

    #[test]
    fn property_successor_strictly_greater_for_every_shape() {
        // Property loop: successor(pred) > pred for canonical, midpoint, "{key}V",
        // decimal, and i64::MAX shapes.
        let shapes: Vec<String> = vec![
            format!("V{}{}", base62_encode_padded(0, 11), "aaaa"),
            format!("V{}{}", base62_encode_padded(1000, 11), "zzzz"),
            order_midpoint("V", "W"),
            order_midpoint("Vaaa", "Vaab"),
            "VabcV".to_string(),
            format!("{:020}", 0),
            format!("{:020}", 999),
            format!("V{}{}", base62_encode_padded(i64::MAX, 11), "0000"),
        ];
        for pred in &shapes {
            for suffix in ["AAAA", "9999", "zzzz"] {
                let succ = order_key_successor(pred, suffix);
                assert!(
                    succ > *pred,
                    "successor({pred:?}, {suffix:?}) = {succ:?} must be > pred"
                );
            }
        }
    }
}

#[cfg(test)]
mod snapshot_threading_tests {
    use super::*;
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContextId, Role, Status, TrackId};

    fn block_id() -> BlockId {
        BlockId::new(ContextId::new(), PrincipalId::new(), 1)
    }

    /// T15 (design §8 Phase 5) — the structural field-drop guard.
    ///
    /// Any new `BlockSnapshot` field MUST be threaded through `BlockContent`
    /// (`from_snapshot` and `snapshot()`) in the same change. This round-trips a
    /// fully-populated snapshot through `BlockContent` and asserts field-by-field
    /// equality — it would have caught option A's dead `played_by` and will catch
    /// the next field that someone forgets to copy. `track` is what this slice
    /// adds; the assertion is deliberately broad so it keeps guarding.
    #[test]
    fn block_snapshot_roundtrips_through_block_content_field_by_field() {
        let id = block_id();
        // Every Option populated so a dropped field shows up as inequality.
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::Model)
            .status(Status::Done)
            .content("hello world")
            .tick(Tick::new(10))
            .track(TrackId::new("bass").unwrap())
            .content_type(ContentType::Markdown)
            .tool_name("shell")
            .tool_input("ls")
            .signature("sig")
            .order_key("V00000000010".to_string())
            .build();

        let principal = snap.id.principal_id;
        let block = BlockContent::from_snapshot(&snap, principal, "fallback".to_string());
        let round = block.snapshot();

        // The field this slice adds — the one the trap was about.
        assert_eq!(round.track, snap.track, "track must survive BlockContent");
        assert_eq!(round.track, Some(TrackId::new("bass").unwrap()));
        // And the broad guard: every content field equal (content_eq now covers
        // track too).
        assert!(
            round.content_eq(&snap),
            "round-tripped snapshot must equal the original field-by-field:\n got {round:#?}\n want {snap:#?}"
        );
        // Explicit spot-checks for the timeline pair (tick + track travel together).
        assert_eq!(round.tick, Some(Tick::new(10)));
        assert_eq!(round.content_type, ContentType::Markdown);
    }

    /// T16(c) (design §8 Phase 5) — a track-bearing snapshot survives the central
    /// CBOR codec round-trip. The pre-track frozen fixture and unknown-key
    /// tolerance live in `kaijutsu_types::codec` tests (the codec's home crate);
    /// this is the crdt-side companion asserting `Some(track)` round-trips.
    #[test]
    fn block_snapshot_cbor_track_round_trip() {
        let snap = BlockSnapshotBuilder::new(block_id(), BlockKind::Text)
            .track(TrackId::new("drums").unwrap())
            .tick(Tick::new(3))
            .build();
        let bytes = kaijutsu_types::codec::encode(&snap).expect("encode");
        let back: BlockSnapshot = kaijutsu_types::codec::decode(&bytes).expect("decode");
        assert_eq!(back.track, Some(TrackId::new("drums").unwrap()));
        assert_eq!(back.tick, Some(Tick::new(3)));
    }
}

#[cfg(test)]
mod text_edit_tests {
    use super::*;
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContextId};

    fn block() -> BlockContent {
        let id = BlockId::new(ContextId::new(), PrincipalId::new(), 1);
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text).build();
        BlockContent::from_snapshot(&snap, id.principal_id, "V".to_string())
    }

    // `pos`/`delete` are CHARACTER offsets, not byte offsets. The regression
    // this pins: a byte-indexed `edit_text` either panics (slicing mid
    // codepoint) or silently corrupts text (wrong byte boundary) on any
    // edit at a position past a multibyte character — exactly the failure
    // class DTE's char-indexed API was hiding and this migration must not
    // reintroduce.

    #[test]
    fn edit_at_interior_position_past_multibyte_chars() {
        let mut b = block();
        b.append_text("日本語 🎵");
        // Chars: 日(0) 本(1) 語(2) ' '(3) 🎵(4) — 5 chars, but the first three
        // are 3 bytes each and 🎵 is 4 bytes, so any byte offset derived from
        // `pos` naively (e.g. `pos` used as a byte index) would split a
        // codepoint. Insert "X" between 語 and the space (char index 3).
        b.edit_text(3, "X", 0);
        assert_eq!(b.text(), "日本語X 🎵");
        assert_eq!(b.content_len(), 6);
    }

    #[test]
    fn delete_spanning_a_multibyte_char() {
        let mut b = block();
        b.append_text("日本語 🎵");
        // Delete "本語" (char indices 1..3) and insert "ABC" in their place.
        b.edit_text(1, "ABC", 2);
        assert_eq!(b.text(), "日ABC 🎵");
        assert_eq!(b.content_len(), 6);
    }

    #[test]
    fn append_after_multibyte_content_lands_at_the_true_end() {
        let mut b = block();
        b.append_text("日本語");
        b.append_text(" 🎵 done");
        assert_eq!(b.text(), "日本語 🎵 done");
    }

    #[test]
    fn edit_text_no_op_on_empty_insert_and_zero_delete() {
        let mut b = block();
        b.append_text("hello");
        b.edit_text(2, "", 0);
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn content_len_counts_characters_not_bytes() {
        let mut b = block();
        b.append_text("日本語"); // 3 chars, 9 bytes
        assert_eq!(b.content_len(), 3);
        assert_eq!(b.text().len(), 9, "sanity: the fixture really is multibyte");
    }
}
