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
    ContentBlock, ContentChunk, SessionId, SessionUpdate, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind as AcpToolKind,
};
use kaijutsu_client::TurnCompletedStopReason;
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, Role, Status, ToolKind as KjToolKind,
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
            .rsplit(|c| c == '.' || c == ':' || c == '/' || c == '_')
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
    /// Used after a CRDT resync: the alternative — [`reset`](Self::reset) and
    /// replay — would re-send the entire transcript to a client that already
    /// has it. Content that changed while we were desynced is lost, which is
    /// why the caller logs loudly. Losing a gap beats duplicating a
    /// conversation.
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
    }

    /// Forget a block entirely (it was deleted).
    pub fn forget(&mut self, block: BlockId) {
        self.emitted.remove(&block);
        self.announced.remove(&block);
        self.tool_status.remove(&block);
    }

    /// Drop every high-water mark. Used after a CRDT resync, where the
    /// document may have been rebuilt underneath us and "what the client has
    /// seen" is no longer a claim we can make about block contents.
    pub fn reset(&mut self) {
        self.emitted.clear();
        self.announced.clear();
        self.tool_status.clear();
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
            // Trace is model-hidden rc plumbing; File/Resource/Task have no
            // honest ACP v1 shape yet (Task wants `plan`, which needs the
            // grooming surface to settle first — docs/acp.md).
            BlockKind::Trace | BlockKind::File | BlockKind::Resource | BlockKind::Task => {
                Vec::new()
            }
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

    #[test]
    fn reset_replays_everything_after_a_resync() {
        let mut m = mapper();
        let b = block(BlockKind::Text, Role::Model, "hello", 1);
        assert_eq!(m.observe(&b).len(), 1);
        m.reset();
        assert_eq!(chunk_text(&m.observe(&b)[0]), "hello");
    }
}
