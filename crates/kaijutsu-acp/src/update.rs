//! Block → ACP `session/update` translation.
//!
//! This is the mapping layer proper, and it is deliberately **pure**: it takes
//! `BlockSnapshot`s (whatever the CRDT currently says a block is) and returns
//! the `SessionUpdate`s the ACP client has not been told about yet. No RPC, no
//! I/O, no clock — so it is unit-testable without a kernel, which is where the
//! tests for this crate live.
//!
//! ## Why "observe a snapshot" rather than "translate an event"
//!
//! kaijutsu's block events are CRDT deltas (`BlockTextOps` carries opaque ops,
//! not text). Decoding them means owning a `SyncedDocument` anyway, and once
//! you own one the block's *current* content is a cheap lookup. So the pump
//! applies the event to the document and then hands the resulting snapshot
//! here; this type keeps a per-block high-water mark of what it already
//! emitted and diffs against it. That makes the translation naturally
//! idempotent — observing an unchanged block twice yields no updates — which
//! also covers replaying history on `session/load` with the same code path.
//!
//! ## Create + patch
//!
//! ACP's `tool_call` / `tool_call_update` pair is a create-then-patch shape,
//! which is exactly `BlockInserted` followed by `BlockStatusChanged` /
//! `BlockOutputChanged`. `ToolCall` blocks announce once and patch after;
//! `ToolResult` blocks patch the ToolCall they point at (`tool_call_id`)
//! rather than opening a second ACP tool call — one kj call/result pair is one
//! ACP tool call.

use std::collections::{HashMap, HashSet};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionId,
    SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind as AcpToolKind,
};
use kaijutsu_client::TurnCompletedStopReason;
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, Role, Status, TaskStatus, ToolKind as KjToolKind,
};

/// Map a kaijutsu turn outcome onto ACP's `stopReason`.
///
/// `subscriptions.rs` says this is 1:1 by construction — the kernel-side enum
/// was designed against ACP v1. Both cancel flavours collapse to `cancelled`
/// because ACP does not distinguish soft from hard; the distinction survives
/// in the kernel (soft leaves a complete phrase, hard leaves a fragment).
pub fn acp_stop_reason(reason: TurnCompletedStopReason) -> StopReason {
    match reason {
        TurnCompletedStopReason::EndTurn => StopReason::EndTurn,
        TurnCompletedStopReason::CancelledSoft | TurnCompletedStopReason::CancelledImmediate => {
            StopReason::Cancelled
        }
        TurnCompletedStopReason::MaxTokens => StopReason::MaxTokens,
        TurnCompletedStopReason::MaxIterations => StopReason::MaxTurnRequests,
    }
}

/// Map kaijutsu block status onto ACP tool-call status. A clean 1:1 — the two
/// enums were arrived at independently and happen to agree.
pub fn acp_tool_status(status: Status) -> ToolCallStatus {
    match status {
        Status::Pending => ToolCallStatus::Pending,
        Status::Running => ToolCallStatus::InProgress,
        Status::Done => ToolCallStatus::Completed,
        Status::Error => ToolCallStatus::Failed,
    }
}

/// Classify a tool for ACP's icon/affordance hint.
///
/// kaijutsu's `ToolKind` says which *engine* ran the tool (shell / MCP /
/// builtin); ACP's `ToolKind` says what the tool *did* (read / edit / search
/// …). They are different axes, so the tool name is the better signal and is
/// consulted first. The engine is the fallback: a shell command is an
/// `execute`, and an unknown MCP or builtin tool is honestly `other`.
pub fn acp_tool_kind(kind: Option<KjToolKind>, name: Option<&str>) -> AcpToolKind {
    if let Some(name) = name {
        // Match on the last path-ish segment so `builtin.file.read`,
        // `kaijutsu:read` and `read` all land the same way.
        let leaf = name
            .rsplit(['.', ':', '/', '_'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        let by_name = match leaf.as_str() {
            "read" | "cat" | "view" => Some(AcpToolKind::Read),
            "edit" | "write" | "patch" | "apply" => Some(AcpToolKind::Edit),
            "delete" | "rm" | "unlink" => Some(AcpToolKind::Delete),
            "move" | "mv" | "rename" => Some(AcpToolKind::Move),
            "search" | "grep" | "glob" | "find" | "index" => Some(AcpToolKind::Search),
            "fetch" | "curl" | "get" | "download" => Some(AcpToolKind::Fetch),
            "think" | "thinking" | "reason" => Some(AcpToolKind::Think),
            "exec" | "execute" | "bash" | "sh" | "shell" | "run" | "kaish" => {
                Some(AcpToolKind::Execute)
            }
            _ => None,
        };
        if let Some(k) = by_name {
            return k;
        }
    }
    match kind {
        Some(KjToolKind::Shell) => AcpToolKind::Execute,
        // MCP and builtin tools are too varied to guess an affordance for.
        Some(KjToolKind::Mcp) | Some(KjToolKind::Builtin) | None => AcpToolKind::Other,
    }
}

/// The ACP tool-call identity for a kj block. `BlockId::to_key()` is the hex
/// form the rest of kaijutsu uses to name a block, so a client that echoes the
/// id back (a permission response, say) hands us something we can parse.
pub fn tool_call_id(block: BlockId) -> ToolCallId {
    ToolCallId::new(block.to_key())
}

/// Per-session translation state: the high-water marks of what this ACP client
/// has already been shown.
#[derive(Debug)]
pub struct UpdateMapper {
    session_id: SessionId,
    /// Chars of `content` already emitted, per block. Char counts, not bytes —
    /// a chunk boundary that lands mid-codepoint is a corrupted frame.
    emitted: HashMap<BlockId, usize>,
    /// Tool calls whose ACP `tool_call` create has gone out; subsequent
    /// observations patch instead of re-announcing.
    announced: HashSet<BlockId>,
    /// Last status pushed per tool call, so an unchanged status does not
    /// generate a redundant patch on every text op.
    tool_status: HashMap<BlockId, ToolCallStatus>,
    /// Blocks this bridge authored itself (the prompt we just submitted). We
    /// suppress the echo so the client does not render its own message twice —
    /// but only for *our* writes. A sibling human typing into the same context
    /// from the desktop app still comes through as a `user_message_chunk`,
    /// which is the crosstalk-as-feature stance, not an accident.
    suppressed: HashSet<BlockId>,
    /// Armed before a `submit_input`, disarmed by the next unseen user block.
    ///
    /// We cannot suppress by block id: `submit_input` only tells us the id
    /// *after* it returns, and the pump can have already streamed the echo by
    /// then. Arming before the write closes that race. The cost is a narrow
    /// window in which a sibling's message could be eaten instead of ours —
    /// accepted for the prototype, and noted in docs/acp.md.
    armed: bool,
    /// Per-Task-block high-water mark: `(content, task_status)` last seen.
    /// This is the mapper's half of the plan story — cheap, per-block
    /// "did this change" detection, mirroring `emitted`/`tool_status` for
    /// every other kind. See [`Self::note_task`].
    task_marks: HashMap<BlockId, (String, TaskStatus)>,
    /// The plan last actually emitted to the client — or silently
    /// established as a baseline by [`Self::baseline_plan`]. The ONE place
    /// plan-level dedup lives: [`Self::build_plan`] diffs against this, so a
    /// rebuild that produces byte-for-byte the same entries (a resync sweep
    /// over unchanged tasks, a duplicate event) emits nothing — the same
    /// "identical re-observation is silent" contract `observe()` gives every
    /// other block kind.
    last_plan: Option<Vec<PlanEntry>>,
}

impl UpdateMapper {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            emitted: HashMap::new(),
            announced: HashSet::new(),
            tool_status: HashMap::new(),
            suppressed: HashSet::new(),
            armed: false,
            task_marks: HashMap::new(),
            last_plan: None,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Mark a block as locally authored — it will not be echoed back to the
    /// client that authored it.
    pub fn suppress(&mut self, block: BlockId) {
        self.suppressed.insert(block);
    }

    /// Arm echo suppression for the next user block we have not seen before.
    /// Call this immediately *before* writing the input doc, not after
    /// `submit_input` returns — see the `armed` field docs.
    pub fn arm_echo_suppression(&mut self) {
        self.armed = true;
    }

    /// Bring the high-water marks up to a block's current state **without
    /// emitting anything**.
    ///
    /// Used when binding a `session/new` to a context whose existing blocks
    /// the client should NOT have narrated at it (rc-seeded bootstrap). Not
    /// used for resync — a resync keeps its marks and re-observes, so the
    /// client receives exactly the gap (see `session.rs::resync`).
    pub fn mark_seen(&mut self, block: &BlockSnapshot) {
        self.emitted.insert(block.id, block.content.chars().count());
        if matches!(block.kind, BlockKind::ToolCall) {
            self.announced.insert(block.id);
            self.tool_status.insert(block.id, acp_tool_status(block.status));
        }
        if matches!(block.kind, BlockKind::ToolResult)
            && let Some(target) = block.tool_call_id
        {
            self.announced.insert(target);
            self.tool_status.insert(target, acp_tool_status(block.status));
        }
        if matches!(block.kind, BlockKind::Task) {
            self.task_marks.insert(block.id, task_mark(block));
        }
    }

    /// Forget a block entirely (it was deleted).
    pub fn forget(&mut self, block: BlockId) {
        self.emitted.remove(&block);
        self.announced.remove(&block);
        self.tool_status.remove(&block);
        self.task_marks.remove(&block);
    }

    /// Drop state for blocks not in `live` — resync hygiene, so marks for
    /// blocks deleted during a desync gap don't accumulate. The surviving
    /// marks are the point: they are what lets a post-resync
    /// [`observe`](Self::observe) sweep emit only the gap instead of the
    /// whole transcript.
    pub fn retain_marks<F: Fn(&BlockId) -> bool>(&mut self, live: F) {
        self.emitted.retain(|id, _| live(id));
        self.announced.retain(|id| live(id));
        self.tool_status.retain(|id, _| live(id));
        self.suppressed.retain(|id| live(id));
        self.task_marks.retain(|id, _| live(id));
    }

    /// Update this Task block's high-water mark; `true` when it's new or its
    /// `(content, task_status)` moved since the last mark, `false` for any
    /// non-`Task` block (harmless no-op, so the pump can call this
    /// unconditionally per event rather than gating on kind itself) or an
    /// unchanged Task block.
    ///
    /// This is the mapper's half of the plan story: "did a Task block change
    /// in a way the client hasn't seen." The pump uses a `true` result to
    /// decide a full plan rebuild ([`task_plan_entries`] via
    /// [`Self::build_plan`]) is worth attempting on the live per-event path.
    /// `session/load` replay and the resync sweep skip this gate — they
    /// already walk every block, so they rebuild unconditionally and let
    /// [`Self::build_plan`]'s own diff against the last emitted plan be the
    /// idempotence guarantee those paths need.
    pub fn note_task(&mut self, block: &BlockSnapshot) -> bool {
        if block.kind != BlockKind::Task {
            return false;
        }
        let mark = task_mark(block);
        let changed = self.task_marks.get(&block.id) != Some(&mark);
        self.task_marks.insert(block.id, mark);
        changed
    }

    /// Rebuild the whole-context task plan from `blocks` (must be document
    /// order — see [`task_plan_entries`]) and return the `plan` update to
    /// send, or `None` when it is identical to the plan already emitted (or
    /// baselined by [`Self::baseline_plan`]).
    ///
    /// This is the ONE rebuild-and-emit path: the live pump (gated by
    /// [`Self::note_task`]), `session/load` replay, and the resync sweep all
    /// call it, so a client always receives the CURRENT full task list —
    /// never a partial patch, never a duplicate.
    pub fn build_plan(&mut self, blocks: &[BlockSnapshot]) -> Option<SessionUpdate> {
        let entries = task_plan_entries(blocks);
        if self.last_plan.as_ref() == Some(&entries) {
            return None;
        }
        self.last_plan = Some(entries.clone());
        Some(SessionUpdate::Plan(Plan::new(entries)))
    }

    /// Establish the plan baseline without emitting — `session/new`
    /// bootstrap, mirroring [`Self::mark_seen`]: rc-seeded Task blocks
    /// should not be narrated at a client that just opened the session.
    pub fn baseline_plan(&mut self, blocks: &[BlockSnapshot]) {
        self.last_plan = Some(task_plan_entries(blocks));
    }

    /// Translate the current state of `block` into the updates not yet sent.
    ///
    /// Returns an empty vec when nothing changed, so it is safe (and cheap) to
    /// call on every event that touches a block.
    pub fn observe(&mut self, block: &BlockSnapshot) -> Vec<SessionUpdate> {
        match block.kind {
            BlockKind::Text | BlockKind::Notification | BlockKind::Drift => {
                self.observe_message(block)
            }
            BlockKind::Thinking => self
                .take_delta(block)
                .map(|d| vec![SessionUpdate::AgentThoughtChunk(text_chunk(&d))])
                .unwrap_or_default(),
            BlockKind::ToolCall => self.observe_tool_call(block),
            BlockKind::ToolResult => self.observe_tool_result(block),
            BlockKind::Error => self.observe_error(block),
            // Task DOES have an honest ACP v1 shape (`plan`) but it's
            // whole-context, not per-block — this pure single-block method
            // can't build it. See `note_task`/`build_plan`/
            // `task_plan_entries` and `session::run_pump`, which rebuild and
            // emit the full plan whenever a Task block changes.
            BlockKind::Task => Vec::new(),
            // Trace is model-hidden rc plumbing; File/Resource have no
            // honest ACP v1 shape yet.
            BlockKind::Trace | BlockKind::File | BlockKind::Resource => Vec::new(),
        }
    }

    fn observe_message(&mut self, block: &BlockSnapshot) -> Vec<SessionUpdate> {
        // Claim the armed suppression for the first unseen user block.
        if self.armed
            && block.role == Role::User
            && block.kind == BlockKind::Text
            && !self.emitted.contains_key(&block.id)
        {
            self.armed = false;
            self.suppressed.insert(block.id);
        }
        if self.suppressed.contains(&block.id) {
            // Still advance the mark so un-suppressing later does not replay
            // the whole block.
            self.emitted.insert(block.id, block.content.chars().count());
            return Vec::new();
        }
        let Some(delta) = self.take_delta(block) else {
            return Vec::new();
        };
        let chunk = text_chunk(&delta);
        match block.role {
            Role::Model | Role::System => vec![SessionUpdate::AgentMessageChunk(chunk)],
            Role::User => vec![SessionUpdate::UserMessageChunk(chunk)],
            // Tool/Asset text outside a ToolResult has no message lane.
            Role::Tool | Role::Asset => Vec::new(),
        }
    }

    fn observe_tool_call(&mut self, block: &BlockSnapshot) -> Vec<SessionUpdate> {
        let id = tool_call_id(block.id);
        let status = acp_tool_status(block.status);
        let raw_input = block
            .tool_input
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());

        if self.announced.insert(block.id) {
            self.tool_status.insert(block.id, status);
            let mut call = ToolCall::new(id, tool_title(block))
                .kind(acp_tool_kind(block.tool_kind, block.tool_name.as_deref()))
                .status(status);
            if let Some(input) = raw_input {
                call = call.raw_input(input);
            }
            return vec![SessionUpdate::ToolCall(call)];
        }

        // Already announced — patch only what moved.
        if self.tool_status.get(&block.id) == Some(&status) {
            return Vec::new();
        }
        self.tool_status.insert(block.id, status);
        vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new().status(status),
        ))]
    }

    fn observe_tool_result(&mut self, block: &BlockSnapshot) -> Vec<SessionUpdate> {
        // A result patches the call it points at. Without the link we have
        // nothing to patch, so the result stands alone as its own ACP tool
        // call — better a duplicate entry than a silently dropped result.
        let target = block.tool_call_id.unwrap_or(block.id);
        let id = tool_call_id(target);

        let status = if block.is_error {
            ToolCallStatus::Failed
        } else {
            acp_tool_status(block.status)
        };

        // Result text is a full replace, not a delta: ACP's `content` field on
        // tool_call_update replaces the array wholesale. Streaming shell output
        // therefore re-sends the accumulated text on each op — chatty, but
        // correct, and it is what the ACP shape actually offers.
        let mut body = block.content.clone();
        if let Some(err) = block.stderr.as_deref().filter(|s| !s.is_empty()) {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(err);
        }

        let changed_status = self.tool_status.get(&target) != Some(&status);
        let changed_body = self.emitted.get(&block.id).copied().unwrap_or(0)
            != block.content.chars().count()
            || (changed_status && !body.is_empty());
        if !changed_status && !changed_body {
            return Vec::new();
        }
        self.tool_status.insert(target, status);
        self.emitted.insert(block.id, block.content.chars().count());

        let mut fields = ToolCallUpdateFields::new().status(status);
        if !body.is_empty() {
            fields = fields.content(vec![ToolCallContent::from(body)]);
        }
        if let Some(code) = block.exit_code {
            fields = fields.raw_output(serde_json::json!({ "exit_code": code }));
        }
        // The target may never have been announced (a result whose call block
        // we missed). ACP tolerates an update for an unknown id less well than
        // a create, so announce first.
        let mut out = Vec::new();
        if self.announced.insert(target) {
            out.push(SessionUpdate::ToolCall(
                ToolCall::new(tool_call_id(target), tool_title(block)).status(status),
            ));
        }
        out.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id, fields,
        )));
        out
    }

    fn observe_error(&mut self, block: &BlockSnapshot) -> Vec<SessionUpdate> {
        // An Error block attached to a tool call is that call failing. A free
        // Error block is something the human needs to see, and ACP v1 has no
        // error lane — an agent message is the honest place for it.
        if let Some(target) = block.tool_call_id {
            let Some(delta) = self.take_delta(block) else {
                return Vec::new();
            };
            self.tool_status.insert(target, ToolCallStatus::Failed);
            return vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id(target),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![ToolCallContent::from(delta)]),
            ))];
        }
        self.take_delta(block)
            .map(|d| vec![SessionUpdate::AgentMessageChunk(text_chunk(&d))])
            .unwrap_or_default()
    }

    /// Whether every message-lane block in `blocks` has been fully emitted
    /// to this client — the turn-end settle check.
    ///
    /// Turn events and block events ride separate ordered lanes since the
    /// FlowBus rework, so `TurnCompleted` can reach the bridge while the
    /// final text chunks are still in flight on the block lane. Answering
    /// `session/prompt` at that instant makes the client finalize its
    /// message widget and the tail lands after the window closed — a
    /// truncated final report (first live hit 2026-08-05, toad). The prompt
    /// handler polls this against the kernel's current snapshot before
    /// responding; message-lane kinds only, since those are what a client
    /// renders inside the prompt's lifetime.
    pub fn caught_up_with(&self, blocks: &[BlockSnapshot]) -> bool {
        blocks.iter().all(|b| match b.kind {
            BlockKind::Text | BlockKind::Thinking | BlockKind::Notification | BlockKind::Drift => {
                self.suppressed.contains(&b.id)
                    || self.emitted.get(&b.id).copied().unwrap_or(0)
                        >= b.content.chars().count()
            }
            _ => true,
        })
    }

    /// The not-yet-emitted suffix of a block's content, advancing the mark.
    /// `None` when nothing is new.
    fn take_delta(&mut self, block: &BlockSnapshot) -> Option<String> {
        let seen = self.emitted.get(&block.id).copied().unwrap_or(0);
        let total = block.content.chars().count();
        if total <= seen {
            // Content shrank (an edit, or a resync rebuilt the block). Re-peg
            // the mark rather than emitting a negative delta; the client keeps
            // the stale tail, which beats a duplicated body.
            if total < seen {
                self.emitted.insert(block.id, total);
            }
            return None;
        }
        self.emitted.insert(block.id, total);
        Some(block.content.chars().skip(seen).collect())
    }
}

/// This block's plan-relevant identity: the fields whose change makes the
/// whole-context plan worth rebuilding.
fn task_mark(block: &BlockSnapshot) -> (String, TaskStatus) {
    (block.content.clone(), block.task_status)
}

/// `TaskStatus` → ACP `PlanEntryStatus`. `None` for `Cancelled` — see
/// [`task_plan_entries`]'s doc comment for why it's omitted rather than
/// mapped onto one of the three ACP states.
fn task_status_to_plan_status(status: TaskStatus) -> Option<PlanEntryStatus> {
    match status {
        TaskStatus::Open => Some(PlanEntryStatus::Pending),
        TaskStatus::InProgress => Some(PlanEntryStatus::InProgress),
        TaskStatus::Done => Some(PlanEntryStatus::Completed),
        TaskStatus::Cancelled => None,
    }
}

/// The nesting prefix for a flattened subtask, repeated once per ancestor
/// level. ACP's `PlanEntry` has no hierarchy field, so nesting rides along
/// on `content` instead of being dropped.
const SUBTASK_PREFIX: &str = "↳ ";

/// Build the ACP plan entries for the CURRENT state of every Task block in
/// `blocks`. `blocks` MUST be in document order (`SyncedDocument::blocks()`/
/// `block_ids_ordered()`) — a raw `BTreeMap` iteration is principal-major
/// and would scramble both subtask nesting and plan order.
///
/// Semantics (docs/acp.md "Task → plan" has the full writeup):
/// - **Cancelled tasks are omitted.** ACP's plan is "what the agent intends
///   to do"; a cancelled task is a groom decision to do nothing further, so
///   showing it as a permanently-unchecked entry would misrepresent intent
///   rather than convey it. `PlanEntryStatus` has no fourth state to be
///   honest about it in-line either way.
/// - **Subtasks flatten** via a pre-order DFS over the `parent_id` DAG
///   (parent, then each child and its own descendants, before the next
///   sibling), nesting rendered as `SUBTASK_PREFIX` repeated once per level
///   on `content`. A Task whose `parent_id` doesn't name another *Task*
///   block in this set (`None`, or pointing at a non-Task block) is treated
///   as a root — defensive, since nothing in `builtin.tasks` promises
///   `parent_id` only ever points at another task.
/// - **Priority defaults to `Medium`** for every entry — kaijutsu Task
///   blocks carry no priority field (docs/tasks.md "Deferred"); this is
///   ACP's required-field filler, not a real signal.
fn task_plan_entries(blocks: &[BlockSnapshot]) -> Vec<PlanEntry> {
    let task_ids: HashSet<BlockId> = blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Task)
        .map(|b| b.id)
        .collect();

    // children[parent] = that parent's direct Task children, document order.
    // `None` is the root bucket (including tasks whose parent_id isn't a
    // Task in this set).
    let mut children: HashMap<Option<BlockId>, Vec<&BlockSnapshot>> = HashMap::new();
    for task in blocks.iter().filter(|b| b.kind == BlockKind::Task) {
        let parent = task.parent_id.filter(|p| task_ids.contains(p));
        children.entry(parent).or_default().push(task);
    }

    fn walk(
        parent: Option<BlockId>,
        depth: usize,
        children: &HashMap<Option<BlockId>, Vec<&BlockSnapshot>>,
        entries: &mut Vec<PlanEntry>,
    ) {
        let Some(kids) = children.get(&parent) else {
            return;
        };
        for task in kids {
            if let Some(status) = task_status_to_plan_status(task.task_status) {
                let content = if depth == 0 {
                    task.content.clone()
                } else {
                    format!("{}{}", SUBTASK_PREFIX.repeat(depth), task.content)
                };
                entries.push(PlanEntry::new(content, PlanEntryPriority::Medium, status));
            }
            walk(Some(task.id), depth + 1, children, entries);
        }
    }

    let mut entries = Vec::new();
    walk(None, 0, &children, &mut entries);
    entries
}

fn text_chunk(text: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

fn tool_title(block: &BlockSnapshot) -> String {
    block
        .tool_name
        .clone()
        .unwrap_or_else(|| match block.tool_kind {
            Some(KjToolKind::Shell) => "shell".to_string(),
            Some(KjToolKind::Mcp) => "mcp tool".to_string(),
            Some(KjToolKind::Builtin) => "builtin".to_string(),
            None => "tool".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::{ContextId, PrincipalId};

    fn block(kind: BlockKind, role: Role, content: &str, seq: u64) -> BlockSnapshot {
        kaijutsu_types::BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), seq),
            kind,
        )
        .role(role)
        .content(content)
        .build()
    }

    fn mapper() -> UpdateMapper {
        UpdateMapper::new(SessionId::new("test-session"))
    }

    /// A `BlockKind::Task` snapshot. `ctx` is shared across a test's tasks so
    /// `parent_id` can name a sibling by `BlockId` (subtask blocks live in
    /// the same context as their parent).
    fn task(
        ctx: ContextId,
        seq: u64,
        content: &str,
        status: TaskStatus,
        parent: Option<BlockId>,
    ) -> BlockSnapshot {
        let mut builder = kaijutsu_types::BlockSnapshotBuilder::new(
            BlockId::new(ctx, PrincipalId::new(), seq),
            BlockKind::Task,
        )
        .role(Role::Tool)
        .content(content)
        .task_status(status);
        if let Some(p) = parent {
            builder = builder.parent_id(p);
        }
        builder.build()
    }

    fn plan_entries(update: &SessionUpdate) -> Vec<PlanEntry> {
        match update {
            SessionUpdate::Plan(plan) => plan.entries.clone(),
            other => panic!("not a plan: {other:?}"),
        }
    }

    /// A task that disappears from the block list must produce a NEW plan.
    ///
    /// The live pump's `BlockDeleted` arm used to `continue` before
    /// `doc.apply_event`, so a deleted block survived in the mirror and the
    /// plan kept listing it until a resync rebuilt the document. The pump now
    /// applies the delete and rebuilds; this pins the half that decides
    /// whether anything is sent — `build_plan` must notice the shorter list
    /// rather than dedupe it away against the previous plan.
    #[test]
    fn build_plan_re_emits_when_a_task_disappears() {
        let ctx = ContextId::new();
        let mut m = mapper();
        let t1 = task(ctx, 1, "first", TaskStatus::Open, None);
        let t2 = task(ctx, 2, "second", TaskStatus::Open, None);

        let both = m
            .build_plan(&[t1.clone(), t2.clone()])
            .expect("first plan must emit");
        assert_eq!(plan_entries(&both).len(), 2);

        // Same list again: nothing changed, so nothing is sent.
        assert!(
            m.build_plan(&[t1.clone(), t2.clone()]).is_none(),
            "an unchanged plan must not be re-sent"
        );

        // t2 deleted — the plan is now shorter and MUST be re-emitted.
        let after = m
            .build_plan(std::slice::from_ref(&t1))
            .expect("a deleted task must produce a new plan");
        let entries = plan_entries(&after);
        assert_eq!(entries.len(), 1, "deleted task must leave the plan");
        assert!(
            entries.iter().all(|e| e.content != "second"),
            "the deleted task must not still be listed: {entries:?}"
        );
    }

    /// Deleting something that was never a task must NOT churn the plan —
    /// the pump calls `build_plan` after every deletion and relies on this
    /// dedupe to stay quiet for ordinary blocks.
    #[test]
    fn build_plan_stays_quiet_when_a_non_task_disappears() {
        let ctx = ContextId::new();
        let mut m = mapper();
        let t1 = task(ctx, 1, "only", TaskStatus::Open, None);
        let chatter = block(BlockKind::Text, Role::Model, "hello", 9);

        assert!(m.build_plan(&[t1.clone(), chatter.clone()]).is_some());
        assert!(
            m.build_plan(std::slice::from_ref(&t1)).is_none(),
            "removing a non-Task block leaves the plan identical, so the pump \
             must send nothing"
        );
    }

    fn chunk_text(u: &SessionUpdate) -> String {
        let c = match u {
            SessionUpdate::AgentMessageChunk(c)
            | SessionUpdate::AgentThoughtChunk(c)
            | SessionUpdate::UserMessageChunk(c) => c,
            other => panic!("not a chunk: {other:?}"),
        };
        match &c.content {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("not text: {other:?}"),
        }
    }

    // ── stop reasons ────────────────────────────────────────────────────────

    #[test]
    fn stop_reasons_map_one_to_one() {
        assert_eq!(
            acp_stop_reason(TurnCompletedStopReason::EndTurn),
            StopReason::EndTurn
        );
        assert_eq!(
            acp_stop_reason(TurnCompletedStopReason::MaxTokens),
            StopReason::MaxTokens
        );
        assert_eq!(
            acp_stop_reason(TurnCompletedStopReason::MaxIterations),
            StopReason::MaxTurnRequests
        );
    }

    #[test]
    fn both_cancel_flavours_collapse_to_cancelled() {
        assert_eq!(
            acp_stop_reason(TurnCompletedStopReason::CancelledSoft),
            StopReason::Cancelled
        );
        assert_eq!(
            acp_stop_reason(TurnCompletedStopReason::CancelledImmediate),
            StopReason::Cancelled
        );
    }

    #[test]
    fn block_status_maps_to_tool_status() {
        assert_eq!(acp_tool_status(Status::Pending), ToolCallStatus::Pending);
        assert_eq!(acp_tool_status(Status::Running), ToolCallStatus::InProgress);
        assert_eq!(acp_tool_status(Status::Done), ToolCallStatus::Completed);
        assert_eq!(acp_tool_status(Status::Error), ToolCallStatus::Failed);
    }

    // ── tool kind classification ────────────────────────────────────────────

    #[test]
    fn tool_name_beats_engine_for_affordance() {
        // A builtin named `read` is a Read, not an Other.
        assert_eq!(
            acp_tool_kind(Some(KjToolKind::Builtin), Some("builtin.file.read")),
            AcpToolKind::Read
        );
        assert_eq!(
            acp_tool_kind(Some(KjToolKind::Mcp), Some("kaijutsu:edit")),
            AcpToolKind::Edit
        );
        assert_eq!(
            acp_tool_kind(Some(KjToolKind::Mcp), Some("ripgrep_search")),
            AcpToolKind::Search
        );
    }

    #[test]
    fn engine_is_the_fallback_when_the_name_says_nothing() {
        assert_eq!(
            acp_tool_kind(Some(KjToolKind::Shell), None),
            AcpToolKind::Execute
        );
        assert_eq!(
            acp_tool_kind(Some(KjToolKind::Mcp), Some("consult")),
            AcpToolKind::Other
        );
        assert_eq!(acp_tool_kind(None, None), AcpToolKind::Other);
    }

    // ── text streaming ──────────────────────────────────────────────────────

    #[test]
    fn assistant_text_streams_as_deltas_not_repeats() {
        let mut m = mapper();
        let mut b = block(BlockKind::Text, Role::Model, "Hel", 1);
        let first = m.observe(&b);
        assert_eq!(chunk_text(&first[0]), "Hel");

        b.content = "Hello there".to_string();
        let second = m.observe(&b);
        assert_eq!(second.len(), 1);
        assert_eq!(chunk_text(&second[0]), "lo there");
    }

    #[test]
    fn observing_an_unchanged_block_emits_nothing() {
        let mut m = mapper();
        let b = block(BlockKind::Text, Role::Model, "done", 1);
        assert_eq!(m.observe(&b).len(), 1);
        assert!(m.observe(&b).is_empty());
    }

    #[test]
    fn deltas_are_char_wise_not_byte_wise() {
        // A byte-indexed delta would slice this mid-codepoint and hand the
        // client a broken frame.
        let mut m = mapper();
        let mut b = block(BlockKind::Text, Role::Model, "日本", 1);
        assert_eq!(chunk_text(&m.observe(&b)[0]), "日本");
        b.content = "日本語".to_string();
        assert_eq!(chunk_text(&m.observe(&b)[0]), "語");
    }

    #[test]
    fn thinking_rides_the_thought_lane() {
        let mut m = mapper();
        let b = block(BlockKind::Thinking, Role::Model, "hmm", 1);
        let out = m.observe(&b);
        assert!(matches!(out[0], SessionUpdate::AgentThoughtChunk(_)));
    }

    #[test]
    fn a_sibling_humans_message_reaches_the_client() {
        // Crosstalk is a feature: a human typing in the desktop app shows up
        // in the phone's transcript.
        let mut m = mapper();
        let b = block(BlockKind::Text, Role::User, "over here", 1);
        let out = m.observe(&b);
        assert!(matches!(out[0], SessionUpdate::UserMessageChunk(_)));
    }

    #[test]
    fn our_own_prompt_is_not_echoed_back() {
        let mut m = mapper();
        let b = block(BlockKind::Text, Role::User, "what we just sent", 1);
        m.suppress(b.id);
        assert!(m.observe(&b).is_empty());
    }

    #[test]
    fn trace_blocks_stay_hidden() {
        let mut m = mapper();
        let b = block(BlockKind::Trace, Role::System, "rc stdout", 1);
        assert!(m.observe(&b).is_empty());
    }

    // ── tool call create + patch ────────────────────────────────────────────

    #[test]
    fn tool_call_announces_once_then_patches() {
        let mut m = mapper();
        let mut b = block(BlockKind::ToolCall, Role::Model, "", 1);
        b.tool_name = Some("grep".into());
        b.tool_kind = Some(KjToolKind::Builtin);
        b.tool_input = Some(r#"{"pattern":"fn main"}"#.into());
        b.status = Status::Running;

        let created = m.observe(&b);
        assert_eq!(created.len(), 1);
        let SessionUpdate::ToolCall(call) = &created[0] else {
            panic!("expected a create, got {:?}", created[0]);
        };
        assert_eq!(call.title, "grep");
        assert_eq!(call.kind, AcpToolKind::Search);
        assert_eq!(call.status, ToolCallStatus::InProgress);
        assert!(call.raw_input.is_some());

        // Same status again: nothing to say.
        assert!(m.observe(&b).is_empty());

        b.status = Status::Done;
        let patched = m.observe(&b);
        assert_eq!(patched.len(), 1);
        let SessionUpdate::ToolCallUpdate(up) = &patched[0] else {
            panic!("expected a patch, got {:?}", patched[0]);
        };
        assert_eq!(up.tool_call_id, tool_call_id(b.id));
        assert_eq!(up.fields.status, Some(ToolCallStatus::Completed));
    }

    #[test]
    fn tool_result_patches_the_call_it_points_at() {
        let mut m = mapper();
        let mut call = block(BlockKind::ToolCall, Role::Model, "", 1);
        call.tool_name = Some("bash".into());
        call.status = Status::Running;
        m.observe(&call);

        let mut result = block(BlockKind::ToolResult, Role::Tool, "ok\n", 2);
        result.tool_call_id = Some(call.id);
        result.status = Status::Done;
        result.exit_code = Some(0);

        let out = m.observe(&result);
        // One patch, addressed to the CALL's id — not a second tool call.
        assert_eq!(out.len(), 1);
        let SessionUpdate::ToolCallUpdate(up) = &out[0] else {
            panic!("expected a patch, got {:?}", out[0]);
        };
        assert_eq!(up.tool_call_id, tool_call_id(call.id));
        assert_eq!(up.fields.status, Some(ToolCallStatus::Completed));
        assert!(up.fields.content.is_some());
    }

    #[test]
    fn an_orphan_tool_result_still_announces_itself() {
        // No `tool_call_id` link — better a standalone entry than a dropped
        // result.
        let mut m = mapper();
        let mut result = block(BlockKind::ToolResult, Role::Tool, "output", 1);
        result.status = Status::Done;
        let out = m.observe(&result);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], SessionUpdate::ToolCall(_)));
        assert!(matches!(out[1], SessionUpdate::ToolCallUpdate(_)));
    }

    #[test]
    fn is_error_forces_failed_even_when_status_says_done() {
        let mut m = mapper();
        let mut result = block(BlockKind::ToolResult, Role::Tool, "boom", 1);
        result.status = Status::Done;
        result.is_error = true;
        let out = m.observe(&result);
        let SessionUpdate::ToolCallUpdate(up) = out.last().unwrap() else {
            panic!("expected a patch");
        };
        assert_eq!(up.fields.status, Some(ToolCallStatus::Failed));
    }

    #[test]
    fn stderr_rides_along_with_stdout() {
        let mut m = mapper();
        let mut result = block(BlockKind::ToolResult, Role::Tool, "out", 1);
        result.stderr = Some("warn".into());
        result.status = Status::Done;
        let out = m.observe(&result);
        let SessionUpdate::ToolCallUpdate(up) = out.last().unwrap() else {
            panic!("expected a patch");
        };
        let content = up.fields.content.as_ref().unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected inline content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text, "out\nwarn");
    }

    #[test]
    fn a_linked_error_block_fails_the_tool_call() {
        let mut m = mapper();
        let call = block(BlockKind::ToolCall, Role::Model, "", 1);
        let mut err = block(BlockKind::Error, Role::System, "permission denied", 2);
        err.tool_call_id = Some(call.id);
        let out = m.observe(&err);
        let SessionUpdate::ToolCallUpdate(up) = &out[0] else {
            panic!("expected a patch, got {:?}", out[0]);
        };
        assert_eq!(up.tool_call_id, tool_call_id(call.id));
        assert_eq!(up.fields.status, Some(ToolCallStatus::Failed));
    }

    #[test]
    fn a_free_error_block_reaches_the_human_as_a_message() {
        let mut m = mapper();
        let err = block(BlockKind::Error, Role::System, "kernel said no", 1);
        let out = m.observe(&err);
        assert_eq!(chunk_text(&out[0]), "kernel said no");
    }

    #[test]
    fn arming_before_the_write_wins_the_race_against_the_pump() {
        // The realistic order: we arm, submit, and the echo arrives before
        // `submit_input` has even returned us a block id.
        let mut m = mapper();
        m.arm_echo_suppression();
        let b = block(BlockKind::Text, Role::User, "our prompt", 1);
        assert!(m.observe(&b).is_empty(), "our own prompt must not echo");
        // Disarmed: the next user block is somebody else's and must come
        // through.
        let other = block(BlockKind::Text, Role::User, "a sibling typed", 2);
        assert_eq!(chunk_text(&m.observe(&other)[0]), "a sibling typed");
    }

    #[test]
    fn arming_does_not_swallow_assistant_text() {
        let mut m = mapper();
        m.arm_echo_suppression();
        let b = block(BlockKind::Text, Role::Model, "thinking out loud", 1);
        assert_eq!(chunk_text(&m.observe(&b)[0]), "thinking out loud");
    }

    #[test]
    fn mark_seen_catches_up_silently_after_a_resync() {
        let mut m = mapper();
        let mut b = block(BlockKind::Text, Role::Model, "already delivered", 1);
        m.mark_seen(&b);
        assert!(
            m.observe(&b).is_empty(),
            "resync must not replay the transcript"
        );
        b.content.push_str(" plus more");
        assert_eq!(chunk_text(&m.observe(&b)[0]), " plus more");
    }

    #[test]
    fn mark_seen_remembers_an_announced_tool_call() {
        let mut m = mapper();
        let mut call = block(BlockKind::ToolCall, Role::Model, "", 1);
        call.status = Status::Running;
        call.tool_name = Some("bash".into());
        m.mark_seen(&call);
        assert!(m.observe(&call).is_empty(), "no duplicate create");
        call.status = Status::Done;
        let out = m.observe(&call);
        assert!(matches!(out[0], SessionUpdate::ToolCallUpdate(_)));
    }

    /// The resync catch-up contract (first live victim 2026-08-05: a FlowBus
    /// lag mid-turn ate the final report text; the old reset+mark_seen
    /// re-pegged silently and the answer never reached toad): marks survive
    /// the resync, so a full observe() sweep of the rebuilt doc emits ONLY
    /// each block's unseen tail — never a duplicate of what was already
    /// delivered, never a re-announce of a known tool call.
    #[test]
    fn resync_sweep_emits_exactly_the_gap() {
        let mut m = mapper();
        let mut b = block(BlockKind::Text, Role::Model, "seen before the lag", 1);
        assert_eq!(m.observe(&b).len(), 1, "delivered pre-gap");
        let mut call = block(BlockKind::ToolCall, Role::Model, "", 2);
        call.status = Status::Running;
        call.tool_name = Some("bash".into());
        assert!(!m.observe(&call).is_empty(), "announced pre-gap");

        // The gap: text grew, the tool call finished, a brand-new block
        // appeared. Then the pump resyncs and sweeps.
        b.content.push_str(" — and the part the gap swallowed");
        call.status = Status::Done;
        let fresh = block(BlockKind::Text, Role::Model, "the final report", 3);

        let mut swept = Vec::new();
        for blk in [&b, &call, &fresh] {
            swept.extend(m.observe(blk));
        }
        assert_eq!(swept.len(), 3, "one tail + one patch + one new block: {swept:?}");
        assert_eq!(chunk_text(&swept[0]), " — and the part the gap swallowed");
        assert!(matches!(swept[1], SessionUpdate::ToolCallUpdate(_)));
        assert_eq!(chunk_text(&swept[2]), "the final report");

        // Idempotent: a second sweep over unchanged state emits nothing.
        let mut again = Vec::new();
        for blk in [&b, &call, &fresh] {
            again.extend(m.observe(blk));
        }
        assert!(again.is_empty(), "second sweep must be silent: {again:?}");
    }

    /// The turn-end settle check: fully-emitted and suppressed message
    /// blocks count as caught up; a block with an undelivered tail does
    /// not; non-message kinds never gate it. This is what `run_turn` polls
    /// before answering `session/prompt`, so `TurnCompleted` beating the
    /// last text chunks across lanes can no longer truncate the final
    /// answer at the client.
    #[test]
    fn caught_up_requires_every_message_tail_delivered() {
        let mut m = mapper();
        let mut b = block(BlockKind::Text, Role::Model, "the final report", 1);
        assert!(!m.caught_up_with(std::slice::from_ref(&b)), "nothing delivered yet");
        m.observe(&b);
        assert!(m.caught_up_with(std::slice::from_ref(&b)), "fully delivered");
        b.content.push_str(" — plus a tail still in flight");
        assert!(!m.caught_up_with(std::slice::from_ref(&b)), "undelivered tail must gate");

        // Suppressed (our own prompt echo) counts as caught up unseen.
        let prompt = block(BlockKind::Text, Role::User, "prompt", 2);
        m.suppress(prompt.id);
        assert!(m.caught_up_with(&[prompt]));

        // Non-message kinds never gate the settle.
        let call = block(BlockKind::ToolCall, Role::Model, "unobserved", 3);
        assert!(m.caught_up_with(&[call]));
    }

    #[test]
    fn retain_marks_prunes_only_dead_blocks() {
        let mut m = mapper();
        let kept = block(BlockKind::Text, Role::Model, "kept", 1);
        let dead = block(BlockKind::Text, Role::Model, "dead", 2);
        m.observe(&kept);
        m.observe(&dead);
        m.retain_marks(|id| *id == kept.id);
        assert!(m.observe(&kept).is_empty(), "kept block's mark must survive");
        assert_eq!(
            chunk_text(&m.observe(&dead)[0]),
            "dead",
            "a pruned block observed again replays from zero (it is new again)"
        );
    }

    // ── task blocks → plan ──────────────────────────────────────────────────

    #[test]
    fn observe_never_emits_a_per_block_update_for_a_task() {
        // Task's shape (`plan`) is whole-context, not per-block — observe()
        // must stay silent for it no matter what changes; the pump builds
        // the plan separately via note_task/build_plan.
        let mut m = mapper();
        let t = task(ContextId::new(), 1, "buy milk", TaskStatus::Open, None);
        assert!(m.observe(&t).is_empty());
    }

    #[test]
    fn task_create_emits_a_plan_containing_it() {
        let ctx = ContextId::new();
        let mut m = mapper();
        let t = task(ctx, 1, "buy milk", TaskStatus::Open, None);

        assert!(m.note_task(&t), "a brand-new task is a change");
        let update = m.build_plan(std::slice::from_ref(&t)).expect("plan emitted");
        let entries = plan_entries(&update);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "buy milk");
        assert_eq!(entries[0].status, PlanEntryStatus::Pending);
        assert_eq!(entries[0].priority, PlanEntryPriority::Medium);
    }

    #[test]
    fn status_change_re_emits_with_the_new_status() {
        let ctx = ContextId::new();
        let mut m = mapper();
        let mut t = task(ctx, 1, "buy milk", TaskStatus::Open, None);
        m.note_task(&t);
        m.build_plan(&[t.clone()]).expect("first plan emitted");

        t.task_status = TaskStatus::InProgress;
        assert!(m.note_task(&t), "status flip is a change");
        let update = m.build_plan(&[t.clone()]).expect("re-emitted");
        assert_eq!(plan_entries(&update)[0].status, PlanEntryStatus::InProgress);

        t.task_status = TaskStatus::Done;
        assert!(m.note_task(&t));
        let update = m.build_plan(&[t]).expect("re-emitted again");
        assert_eq!(plan_entries(&update)[0].status, PlanEntryStatus::Completed);
    }

    #[test]
    fn a_non_task_change_emits_no_plan() {
        // note_task is the pump's gate: it must be a harmless no-op for any
        // other block kind, so a Text/ToolCall/whatever event never triggers
        // a plan rebuild.
        let mut m = mapper();
        let text = block(BlockKind::Text, Role::Model, "hello", 1);
        assert!(!m.note_task(&text));
        let tool_call = block(BlockKind::ToolCall, Role::Model, "", 2);
        assert!(!m.note_task(&tool_call));
    }

    #[test]
    fn identical_reobservation_emits_no_duplicate_plan() {
        // The resync sweep's contract: rebuilding from unchanged task state
        // must be silent, same as observe()'s idempotence for every other
        // kind.
        let ctx = ContextId::new();
        let mut m = mapper();
        let t = task(ctx, 1, "buy milk", TaskStatus::Open, None);
        m.note_task(&t);
        assert!(m.build_plan(std::slice::from_ref(&t)).is_some(), "first build emits");
        assert!(
            m.build_plan(&[t]).is_none(),
            "second build over unchanged state must be silent"
        );
    }

    #[test]
    fn load_replay_emits_exactly_one_plan_at_the_end() {
        // Mirrors run_pump's replay loop: observe() every block first (Task
        // never emits per-block), THEN one build_plan call over the whole
        // set — not one per task touched during replay.
        let ctx = ContextId::new();
        let mut m = mapper();
        let blocks = vec![
            task(ctx, 1, "buy milk", TaskStatus::Open, None),
            task(ctx, 2, "walk the dog", TaskStatus::Done, None),
        ];

        let mut replayed = Vec::new();
        for b in &blocks {
            replayed.extend(m.observe(b));
        }
        assert!(replayed.is_empty(), "no per-block updates during replay");

        let plan = m.build_plan(&blocks).expect("exactly one plan at the end");
        let entries = plan_entries(&plan);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "buy milk");
        assert_eq!(entries[1].content, "walk the dog");

        // A second identical rebuild (e.g. a resync sweep immediately after
        // load with no task activity) must not duplicate it.
        assert!(m.build_plan(&blocks).is_none());
    }

    #[test]
    fn cancelled_tasks_are_omitted_from_the_plan() {
        let ctx = ContextId::new();
        let blocks = vec![
            task(ctx, 1, "buy milk", TaskStatus::Open, None),
            task(ctx, 2, "cancelled errand", TaskStatus::Cancelled, None),
        ];
        let entries = task_plan_entries(&blocks);
        assert_eq!(entries.len(), 1, "the cancelled task must not appear: {entries:?}");
        assert_eq!(entries[0].content, "buy milk");
    }

    #[test]
    fn a_task_cancelled_after_being_shown_disappears_on_the_next_plan() {
        // Cancelling isn't a delete — it's a status write like any other, so
        // it must round-trip through the normal note_task/build_plan path
        // and the entry must vanish from the rebuilt plan.
        let ctx = ContextId::new();
        let mut m = mapper();
        let mut t = task(ctx, 1, "buy milk", TaskStatus::Open, None);
        m.note_task(&t);
        let first = m.build_plan(&[t.clone()]).unwrap();
        assert_eq!(plan_entries(&first).len(), 1);

        t.task_status = TaskStatus::Cancelled;
        assert!(m.note_task(&t), "cancelling is a status change");
        let second = m.build_plan(&[t]).expect("plan changed — the entry dropped");
        assert!(plan_entries(&second).is_empty());
    }

    #[test]
    fn subtasks_flatten_in_dag_order_with_a_nesting_prefix() {
        let ctx = ContextId::new();
        let parent = task(ctx, 1, "plan the trip", TaskStatus::Open, None);
        let child_a = task(ctx, 2, "book flights", TaskStatus::Open, Some(parent.id));
        let grandchild = task(ctx, 3, "pick seats", TaskStatus::Open, Some(child_a.id));
        let child_b = task(ctx, 4, "book hotel", TaskStatus::Open, Some(parent.id));
        let other_root = task(ctx, 5, "unrelated errand", TaskStatus::Open, None);

        // Deliberately out of document order relative to the DAG to prove
        // flattening follows parent_id, not insertion position.
        let blocks = vec![
            parent.clone(),
            other_root,
            child_a.clone(),
            grandchild,
            child_b,
        ];
        let entries = task_plan_entries(&blocks);
        let contents: Vec<&str> = entries.iter().map(|e| e.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "plan the trip",
                "↳ book flights",
                "↳ ↳ pick seats",
                "↳ book hotel",
                "unrelated errand",
            ],
            "parent, then its subtree depth-first, then the next root"
        );
    }

    #[test]
    fn an_orphaned_parent_id_falls_back_to_root() {
        // A task whose parent_id points at a non-Task block (or a Task not
        // in this snapshot) must not be silently dropped from the plan.
        let ctx = ContextId::new();
        let stray_parent = BlockId::new(ctx, PrincipalId::new(), 99);
        let t = task(ctx, 1, "orphan", TaskStatus::Open, Some(stray_parent));
        let entries = task_plan_entries(std::slice::from_ref(&t));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "orphan", "no nesting prefix — treated as root");
    }

    #[test]
    fn session_new_bootstrap_baselines_silently() {
        // mark_seen + baseline_plan (the session/new path) must not treat a
        // pre-existing rc-seeded task as new; the client should not be
        // narrated at for state that predates its connection.
        let ctx = ContextId::new();
        let mut m = mapper();
        let t = task(ctx, 1, "pre-seeded", TaskStatus::Open, None);
        m.mark_seen(&t);
        m.baseline_plan(std::slice::from_ref(&t));

        assert!(
            m.build_plan(&[t]).is_none(),
            "no drift from the baseline — nothing to emit"
        );
    }
}
