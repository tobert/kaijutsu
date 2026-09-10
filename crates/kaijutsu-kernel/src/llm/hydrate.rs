//! Block → message translation for the LLM bound conversation.
//!
//! `HydrationState` is a small state machine that accumulates blocks into
//! the message sequence the LLM API expects (alternating
//! user/assistant, with `tool_use` and `tool_result` blocks paired
//! across adjacent messages).
//!
//! Two callers:
//!
//! - **Bootstrap.** `super::hydrate_from_blocks` walks a full block
//!   slice once at boundary events (fork, new context, cold start,
//!   attach) and returns the resulting `Vec<Message>`.
//! - **Incremental.** The per-context mailbox folds unseen blocks from
//!   the durable log on `catch_up`, keeping the live session in sync
//!   without rebuilding from scratch each turn.
//!
//! Both paths share the same `translate_block` / `into_messages` pair
//! so the wire-history contract stays identical.

use std::collections::{BTreeSet, HashMap, HashSet};

use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContentType, Role as BlockRole};

use super::{ContentBlock, LlmError, LlmResult, Message, MessageContent, Role};

/// Project a declared-`Diff` block's content into the body a model reads.
///
/// **Hydration is a projection, not a passthrough.** The canonical block always
/// keeps the whole diff; this is what one consumer — the model — gets, and it
/// is built to three rules (`docs/diff.md`, `kaijutsu_diff`'s truncation
/// contract):
///
/// 1. **The diffstat leads, and it describes the *whole* diff.** A model that
///    reads `12 files, +900 −40` knows the size of the change even when the
///    body below it is a fraction of that.
/// 2. **The budget is spent on whole hunks** via
///    [`truncate_to_bytes`](kaijutsu_diff::truncate_to_bytes), never on
///    characters. A character-count cut leaves a `@@` header whose counts no
///    longer match its body: a patch that looks real and applies wrong, which
///    for a diff is worse than showing nothing. When anything is dropped the
///    formatter's `#!kaijutsu-diff truncated:` marker leads the text and the
///    result still parses — incompleteness is explicit, not inferred.
/// 3. **Content that does not parse is still shown, as plain text.** Content
///    and content-type are separate LWW registers, so a block can honestly
///    declare itself a diff while holding something else. Dropping it would
///    lose the model's own output; crashing hydration over it would take down
///    the turn. It gets a note saying what happened and rides the ordinary
///    character budget — at that point it *is* plain text, and the whole-hunk
///    rule has nothing to protect.
pub(crate) fn project_diff_for_hydration(content: &str) -> String {
    match kaijutsu_diff::parse(content) {
        Ok(model) => {
            // Stat of the complete model, deliberately — the projection below
            // may drop hunks, but the size of the change is not a detail the
            // model should have to infer from what survived.
            let stat = model.stat();
            let projected =
                kaijutsu_diff::truncate_to_bytes(&model, kaijutsu_diff::limits::MAX_HYDRATION_BYTES);
            format!("{stat}\n{}", kaijutsu_diff::format(&projected))
        }
        Err(e) => {
            let budget = kaijutsu_types::TOOL_CONTENT_HYDRATION_BUDGET;
            let body = if content.chars().count() > budget {
                let head: String = content.chars().take(budget).collect();
                format!("{head}\n...[truncated]")
            } else {
                content.to_string()
            };
            format!("[declared as a diff but does not parse ({e}); shown as plain text]\n{body}")
        }
    }
}

/// Accumulates blocks into outgoing `Message`s. Held across multiple
/// `translate_block` calls; consumed by `into_messages`.
///
/// `Clone` is implemented so a live session (`ConversationMailbox`)
/// can take a non-destructive snapshot for send-time repair without
/// losing its in-progress accumulator state.
#[derive(Clone)]
pub(crate) struct HydrationState {
    messages: Vec<Message>,
    assistant_text: Option<String>,
    /// Reasoning accumulated for the in-progress assistant turn — one
    /// `(text, signature)` entry **per** Thinking block, in order. Emitted as
    /// separate `Reasoning` blocks ahead of the turn's text (a turn's thinking
    /// blocks are *not* merged — Anthropic verifies each signature against its
    /// own block). Only signed Thinking blocks land here; signatureless ones
    /// are dropped.
    assistant_reasoning: Vec<(String, Option<String>)>,
    tool_uses: Vec<ContentBlock>,
    tool_results: Vec<ContentBlock>,
    /// Pending user-initiated shell commands, keyed by ToolCall BlockId.
    /// Matched to ToolResults via `tool_call_id` for correct pairing
    /// even when blocks interleave with model tool calls.
    user_shell_pending: HashMap<BlockId, String>,
}

/// Whether a block belongs in model-visible history.
///
/// Distillation summarizes the same durable conversation a later turn would
/// hydrate, so it uses this predicate too. Keep the predicate independent of
/// provider message assembly: callers that need a transcript retain their own
/// formatting while sharing the admission rule.
pub(crate) fn is_history_eligible(block: &BlockSnapshot) -> bool {
    if block.ephemeral || block.status == kaijutsu_types::Status::Draft || block.excluded {
        return false;
    }
    if matches!(block.kind, BlockKind::File | BlockKind::Trace) {
        return false;
    }
    if block.role == BlockRole::System
        && block.kind != BlockKind::Drift
        && block.kind != BlockKind::Error
        && block.kind != BlockKind::Notification
        && block.kind != BlockKind::Resource
        && block.kind != BlockKind::Task
    {
        return false;
    }
    !block.content.is_empty()
        || matches!(
            block.kind,
            BlockKind::ToolCall
                | BlockKind::ToolResult
                | BlockKind::Error
                | BlockKind::Notification
                | BlockKind::Resource
                | BlockKind::Task
        )
}

impl HydrationState {
    pub(crate) fn new() -> Self {
        Self {
            messages: Vec::new(),
            assistant_text: None,
            assistant_reasoning: Vec::new(),
            tool_uses: Vec::new(),
            tool_results: Vec::new(),
            user_shell_pending: HashMap::new(),
        }
    }

    /// Fold one block into the in-progress session.
    ///
    /// `parent` is only consulted for `BlockKind::Error` blocks, which
    /// can fold their content into the parent `ToolResult` if it
    /// hasn't been flushed yet. Pass `None` when the parent isn't
    /// available or known — the Error block falls back to a standalone
    /// user message.
    pub(crate) fn translate_block(
        &mut self,
        block: &BlockSnapshot,
        parent: Option<&BlockSnapshot>,
    ) {
        if !is_history_eligible(block) {
            return;
        }

        match (block.role, block.kind) {
            (BlockRole::User, BlockKind::Text) => {
                self.flush_all();
                self.messages.push(Message::user(&block.content));
            }
            (BlockRole::User, BlockKind::ToolCall) => {
                // User-initiated shell command — extract the code and wait for
                // the paired ToolResult to emit a single user message.
                self.flush_all();
                let code = block
                    .tool_input
                    .as_ref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .and_then(|v| v.get("code").and_then(|c| c.as_str().map(String::from)))
                    .unwrap_or_else(|| block.content.clone());
                self.user_shell_pending.insert(block.id, code);
            }
            (_, BlockKind::Thinking) => {
                // Rehydrate reasoning *only* when the block carries a continuity
                // signature — the opaque marker that says "this is rehydratable"
                // (real Anthropic/Gemini signature, or a DeepSeek nonce). A
                // signatureless Thinking block (generic/local model, or a
                // legacy/older-wire block) is dropped, preserving prior behavior:
                // Anthropic rejects a thinking block echoed back without a valid
                // signature. Cross-provider safety (e.g. not feeding a DeepSeek
                // nonce to Anthropic) is a fork/rc-policy concern handled above
                // the kernel, so the token is treated as opaque here.
                let Some(signature) = block.signature.clone() else {
                    return;
                };
                // A new assistant turn begins — flush any pending tool results
                // from the prior turn, same as Model/Text below.
                self.flush_tool_results();
                // One entry per Thinking block — preserved separately so each
                // signature stays paired with its own text (no merging).
                self.assistant_reasoning
                    .push((block.content.clone(), Some(signature)));
            }
            (BlockRole::Model, BlockKind::Text) => {
                // Flush pending tool results before accumulating assistant text
                self.flush_tool_results();
                match &mut self.assistant_text {
                    Some(text) => {
                        text.push('\n');
                        text.push_str(&block.content);
                    }
                    None => {
                        self.assistant_text = Some(block.content.clone());
                    }
                }
            }
            (BlockRole::Model, BlockKind::ToolCall) => {
                // Flush pending tool results before accumulating tool uses
                self.flush_tool_results();
                let tool_use_id = block.tool_use_id.clone().unwrap_or_else(|| {
                    tracing::warn!(
                        block_id = %block.id.to_key(),
                        "ToolCall block missing tool_use_id, falling back to block ID"
                    );
                    block.id.to_key()
                });
                let name = block.tool_name.clone().unwrap_or_default();
                let input = block
                    .tool_input
                    .as_ref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null);
                self.tool_uses.push(ContentBlock::ToolUse {
                    id: tool_use_id,
                    name,
                    input,
                });
            }
            (BlockRole::Asset, BlockKind::Text) => {
                // img_block / img_block_from_path — Asset role, content_type
                // Image, content holds the CAS hash. Surface to vision-capable
                // models as an Image content block; the server-side path
                // resolves the hash to bytes before the request goes out.
                if block.content_type == ContentType::Image {
                    self.flush_all();
                    self.messages.push(Message {
                        role: Role::User,
                        content: MessageContent::Blocks(vec![ContentBlock::Image {
                            hash: block.content.clone(),
                            media_type: ContentType::Image.as_mime().to_string(),
                            data_base64: None,
                        }]),
                    });
                }
                // Other Asset content types stay skipped (no current producer).
            }
            (BlockRole::Tool, BlockKind::Text) => {
                // Tool-authored rich content (svg_block / abc_block /
                // diff_block). Surface as a user message envelope so the
                // model can read back its own output on the next turn (A1).
                // Plain text from tools stays skipped — only typed content
                // (Svg/Abc/Diff) is worth round-tripping.
                match block.content_type {
                    // Diff gets the same envelope but its own body: a diffstat
                    // plus whole-hunk-bounded content. See
                    // [`project_diff_for_hydration`] for why the generic
                    // char-count truncation is unsafe here specifically.
                    ContentType::Diff => {
                        let body = project_diff_for_hydration(&block.content);
                        let envelope = kaijutsu_types::format_tool_content_envelope(block, &body);
                        self.flush_all();
                        self.messages.push(Message::user(envelope));
                    }
                    ContentType::Svg | ContentType::Abc => {
                        let envelope = kaijutsu_types::format_tool_content_for_llm(block);
                        self.flush_all();
                        self.messages.push(Message::user(envelope));
                    }
                    _ => {
                        // Skip — no rich content to surface.
                    }
                }
            }
            (BlockRole::Tool, BlockKind::ToolResult) => {
                // A shell command can emit typed content — `kj diff` sets the
                // ToolResult block's content_type to `text/x-diff`. That is the
                // *other* producer of diff blocks, and it enters hydration here
                // rather than through the (Tool, Text) arm above, so the same
                // projection has to be applied on both roads. Everything else
                // passes through as the command wrote it.
                // Empty content is left alone: an interrupted or still-empty
                // ToolResult is not a zero-file diff, and saying "0 files,
                // +0 −0" would invent a fact.
                let projected = (block.content_type == ContentType::Diff
                    && !block.content.is_empty())
                .then(|| project_diff_for_hydration(&block.content));
                let stdout = projected.as_deref().unwrap_or(&block.content);

                let user_code = block
                    .tool_call_id
                    .and_then(|id| self.user_shell_pending.remove(&id));
                if let Some(code) = user_code {
                    // User-initiated shell result → emit as a single user message
                    self.flush_all();
                    let output = stdout.trim();
                    if output.is_empty() {
                        self.messages
                            .push(Message::user(format!("[User ran `{}`]", code)));
                    } else {
                        self.messages
                            .push(Message::user(format!("[User ran `{}`]\n{}", code, output)));
                    }
                } else {
                    // Agent-initiated tool result — existing logic
                    self.flush_assistant();
                    let tool_use_id = block
                        .tool_use_id
                        .clone()
                        .or_else(|| {
                            tracing::warn!(
                                block_id = %block.id.to_key(),
                                "ToolResult block missing tool_use_id, falling back to tool_call_id"
                            );
                            block.tool_call_id.map(|id| id.to_key())
                        })
                        .unwrap_or_else(|| {
                            tracing::warn!(
                                block_id = %block.id.to_key(),
                                "ToolResult block missing both tool_use_id and tool_call_id, falling back to block ID"
                            );
                            block.id.to_key()
                        });
                    // stdout lives in `content`, stderr in its own field. The
                    // model needs both — merge them back the way they were
                    // before stderr was split off (stdout, then stderr).
                    let content = match block.stderr.as_deref() {
                        Some(err) if !err.is_empty() && !stdout.is_empty() => {
                            format!("{stdout}\n{err}")
                        }
                        Some(err) if !err.is_empty() => err.to_string(),
                        _ => stdout.to_string(),
                    };
                    self.tool_results.push(ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: block.is_error,
                    });
                }
            }
            (_, BlockKind::Drift) => {
                // Drift blocks become user messages with provenance context
                let source_label = block
                    .source_context
                    .map(|id| id.short())
                    .unwrap_or_else(|| "unknown".to_string());
                let drift_kind = block.drift_kind.map(|k| k.as_str()).unwrap_or("drift");
                let prefixed = format!(
                    "[{} from context {}]\n\n{}",
                    drift_kind, source_label, block.content
                );
                self.flush_all();
                self.messages.push(Message::user(&prefixed));
            }
            (_, BlockKind::Error) => {
                // Error blocks: fold into parent ToolResult content if possible,
                // otherwise emit as standalone user message.
                let envelope = kaijutsu_types::format_error_for_llm(block);

                let parent_is_tool_result =
                    parent.is_some_and(|p| p.kind == BlockKind::ToolResult);

                if parent_is_tool_result {
                    let parent_tool_use_id = parent.and_then(|p| p.tool_use_id.as_deref());

                    let folded = if let Some(target_id) = parent_tool_use_id {
                        self.tool_results
                            .iter_mut()
                            .find_map(|tr| {
                                if let ContentBlock::ToolResult {
                                    tool_use_id,
                                    content,
                                    ..
                                } = tr
                                {
                                    if tool_use_id == target_id {
                                        content.push_str("\n\n");
                                        content.push_str(&envelope);
                                        Some(())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .is_some()
                    } else {
                        false
                    };

                    if !folded {
                        // Parent's tool_result already flushed or not found — standalone
                        self.flush_all();
                        self.messages.push(Message::user(envelope));
                    }
                } else {
                    self.flush_all();
                    self.messages.push(Message::user(envelope));
                }
            }
            (_, BlockKind::Notification) => {
                // Notification blocks (D-34): surface broker tool/log events to the
                // LLM as a user message so the model sees tool-world changes.
                let envelope = kaijutsu_types::format_notification_for_llm(block);
                self.flush_all();
                self.messages.push(Message::user(envelope));
            }
            (_, BlockKind::Resource) => {
                // Resource blocks (D-34, D-43): surface MCP resource contents to
                // the LLM as a user message with an XML envelope so the model sees
                // the read-through body (truncated per
                // RESOURCE_CONTENT_HYDRATION_BUDGET).
                let envelope = kaijutsu_types::format_resource_for_llm(block);
                self.flush_all();
                self.messages.push(Message::user(envelope));
            }
            (_, BlockKind::Task) => {
                // Task blocks (household-agent grooming — docs/tasks.md):
                // surface current status/content to the model as a one-time
                // appended user message. Deliberately mirrors Notification's
                // hydration precedent (D-34) rather than inventing a new
                // mechanism: `translate_block` runs at most once per BlockId
                // (see `ConversationMailbox::feed`/`catch_up`'s `seen` set),
                // so a LATER status/text edit on this SAME task block never
                // rewrites this message — the already-hydrated (and
                // possibly already-cached) prefix stays byte-identical. A
                // status change reaches the model instead through whichever
                // path caused it: the model's own tool_call/tool_result
                // (ordinary hydration, no special-casing needed) if it made
                // the change itself, or — not implemented by this slice — a
                // fresh companion Notification block for an out-of-band
                // groom from another principal. See docs/tasks.md
                // "Hydration" for the full reasoning and what's deferred.
                let envelope = kaijutsu_types::format_task_for_llm(block);
                self.flush_all();
                self.messages.push(Message::user(envelope));
            }
            _ => {
                // Skip unexpected role/kind combinations
            }
        }
    }

    /// Consume the state and repair tool pairing in one message-sequence walk.
    ///
    /// Each assistant call batch is closed before another message is emitted:
    /// keep adjacent results, synthesize errors for missing results, and drop
    /// orphaned results. Non-result content follows the completed result batch.
    /// Duplicate calls/results are kept for the send-time validator to refuse;
    /// choosing one could hide conflicting calls or output.
    pub(crate) fn into_messages(mut self) -> Vec<Message> {
        self.flush_all();
        let mut repaired = Vec::with_capacity(self.messages.len() + 4);
        let mut pending = Vec::new();

        for (msg_idx, mut message) in self.messages.into_iter().enumerate() {
            let required = std::mem::take(&mut pending);
            if message.role == Role::User
                && let MessageContent::Blocks(blocks) = message.content
            {
                let expected: HashSet<&str> = required.iter().map(String::as_str).collect();
                let mut results = Vec::new();
                let mut content = Vec::new();
                for block in blocks {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = &block {
                        if expected.contains(tool_use_id.as_str()) {
                            results.push(block);
                        } else {
                            tracing::warn!(
                                msg_idx,
                                tool_use_id,
                                "hydration repair: dropping orphaned tool_result (late arrival)"
                            );
                        }
                    } else {
                        content.push(block);
                    }
                }
                let has_results = !results.is_empty();
                complete_tool_results(&required, &mut results, msg_idx);
                if has_results {
                    results.extend(content);
                    repaired.push(Message::tool_results(results));
                } else {
                    if !results.is_empty() {
                        repaired.push(Message::tool_results(results));
                    }
                    if !content.is_empty() {
                        message.content = MessageContent::Blocks(content);
                        repaired.push(message);
                    }
                }
                continue;
            }

            let mut results = Vec::new();
            complete_tool_results(&required, &mut results, msg_idx);
            if !results.is_empty() {
                repaired.push(Message::tool_results(results));
            }
            if message.role == Role::Assistant
                && let MessageContent::Blocks(blocks) = &message.content
            {
                pending.extend(blocks.iter().filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                }));
            }
            repaired.push(message);
        }

        let mut results = Vec::new();
        complete_tool_results(&pending, &mut results, repaired.len());
        if !results.is_empty() {
            repaired.push(Message::tool_results(results));
        }
        repaired
    }

    /// Flush any pending assistant reasoning + text + tool_uses into a message.
    fn flush_assistant(&mut self) {
        // Always reset reasoning, even on the drop paths below.
        let reasoning = std::mem::take(&mut self.assistant_reasoning);
        if self.assistant_text.is_none() && self.tool_uses.is_empty() {
            // Lone Reasoning blocks can't stand as an assistant message (the
            // API requires accompanying text or tool_use), so they're dropped.
            return;
        }
        let text = self.assistant_text.take();
        let tool_uses = std::mem::take(&mut self.tool_uses);
        if reasoning.is_empty() && tool_uses.is_empty() {
            // Plain text assistant message — keep the simple Text representation
            // so existing single-text-turn behavior is unchanged.
            if let Some(text) = text {
                self.messages.push(Message::assistant(text));
            }
        } else {
            // Reasoning and/or tool uses present: emit a Blocks message with
            // reasoning first, then text, then tool uses.
            self.messages.push(Message::with_reasoning_text_and_tool_uses(
                reasoning, text, tool_uses,
            ));
        }
    }

    /// Flush any pending tool results into a user message.
    fn flush_tool_results(&mut self) {
        if self.tool_results.is_empty() {
            return;
        }
        let results = std::mem::take(&mut self.tool_results);
        self.messages.push(Message::tool_results(results));
    }

    /// Flush everything pending (assistant then tool results).
    fn flush_all(&mut self) {
        self.flush_assistant();
        self.flush_tool_results();
    }

    /// Inject a synthetic user-role seam marking `archived` source blocks
    /// dropped between two kept runs of a windowed/forked hydration. Flushes
    /// pending assistant/tool state first so the seam lands on a clean message
    /// boundary. Safe to follow a `tool_result` (also a user message): the wire
    /// API merges consecutive same-role messages into one turn. The splicer
    /// ([`super::splice`]) decides *where* a seam belongs; this just renders it.
    pub(crate) fn push_seam(&mut self, archived: usize) {
        self.flush_all();
        self.messages
            .push(Message::user(format!("[{archived} blocks archived]")));
    }
}

/// Flat per-image token cost used by [`estimate_tokens`]. Hydration leaves
/// `data_base64` unresolved (`None`) on every `ContentBlock::Image` — the
/// bytes live in CAS and aren't reachable from a pure `&[Message]` walk — so
/// counting an image as 0 tokens would just lie about its real cost. ~1600
/// tokens is a defensible flat estimate for a typical image attachment
/// (comparable to a standard-resolution screenshot under most vision
/// tokenizers), not a measurement of the actual bytes.
pub(crate) const ESTIMATED_TOKENS_PER_IMAGE: u64 = 1600;

/// Rough bytes-per-token ratio for the character-count → token-count
/// conversion in [`estimate_tokens`]. English prose runs close to 4
/// bytes/token; code and dense structured text (JSON tool input, diffs) run
/// denser, so this estimator undercounts on code-heavy turns. That's why the
/// caller (`llm_stream.rs`'s `CONTEXT_WARNING_THRESHOLD`) warns at 90% of the
/// window rather than 100% — headroom for exactly this undercount.
const BYTES_PER_TOKEN: u64 = 4;

/// Small fixed overhead added per message (role marker, envelope framing)
/// that a raw content-length walk alone would miss.
const PER_MESSAGE_TOKEN_OVERHEAD: u64 = 4;

fn bytes_to_tokens(bytes: usize) -> u64 {
    (bytes as u64) / BYTES_PER_TOKEN
}

fn estimate_block(block: &ContentBlock) -> u64 {
    match block {
        ContentBlock::Text { text } => bytes_to_tokens(text.len()),
        ContentBlock::Reasoning { text, .. } => bytes_to_tokens(text.len()),
        ContentBlock::ToolUse { name, input, .. } => {
            // Serialized JSON length + the tool name — a failed serialization
            // (shouldn't happen for a `serde_json::Value` we built ourselves)
            // degrades to 0 extra bytes rather than panicking; this is an
            // estimate, not a correctness-critical path.
            let input_len = serde_json::to_string(input).map(|s| s.len()).unwrap_or(0);
            bytes_to_tokens(name.len() + input_len)
        }
        ContentBlock::ToolResult { content, .. } => bytes_to_tokens(content.len()),
        ContentBlock::Image { .. } => ESTIMATED_TOKENS_PER_IMAGE,
    }
}

fn estimate_message_content(content: &MessageContent) -> u64 {
    match content {
        MessageContent::Text(text) => bytes_to_tokens(text.len()),
        MessageContent::Blocks(blocks) => blocks.iter().map(estimate_block).sum(),
    }
}

/// Estimate the token cost of a hydrated message sequence *before* it goes
/// out to the provider — a cheap pre-flight sizing check
/// (`llm_stream.rs`'s pre-send context-window warning), not a real
/// tokenizer. Deliberately simple and provider-agnostic: each provider's
/// actual BPE tokenizer differs and a real count would mean a network round
/// trip per turn, whereas a rough byte-derived number is free and fast
/// enough to run unconditionally.
///
/// Never trims, refuses, or mutates anything — purely informational.
pub(crate) fn estimate_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| PER_MESSAGE_TOKEN_OVERHEAD + estimate_message_content(&m.content))
        .sum()
}

/// Close a call batch with explicit errors for every missing result.
fn complete_tool_results(required: &[String], results: &mut Vec<ContentBlock>, msg_idx: usize) {
    let covered: HashSet<String> = results.iter().filter_map(|block| match block {
        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
        _ => None,
    }).collect();
    for id in required {
        if !covered.contains(id) {
            tracing::warn!(
                msg_idx,
                tool_use_id = id,
                "hydration repair: synthesizing tool_result for orphaned tool_use"
            );
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: "Tool execution was interrupted (context was forked or pruned)".into(),
                is_error: true,
            });
        }
    }
}

fn pairing_error(msg_idx: usize, detail: impl std::fmt::Display) -> LlmError {
    LlmError::InvalidRequest(format!(
        "Conversation tool pairing is invalid at message {msg_idx}: {detail}. \
         The turn was stopped before contacting the model. \
         Exclude the offending blocks and fork to rebuild conversation history."
    ))
}

/// Refuse malformed history before provider dispatch, including live appends.
///
/// Every assistant tool_use has exactly one result in the immediately next
/// user message. Results precede ordinary content; both roles and both
/// directions of the pairing are checked. This does not repair live history.
pub(crate) fn validate_tool_pairing(messages: &[Message]) -> LlmResult<()> {
    let mut pending = BTreeSet::new();
    for (msg_idx, message) in messages.iter().enumerate() {
        let mut unanswered = std::mem::take(&mut pending);
        let mut answered = HashSet::new();
        let mut saw_content = false;
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        if message.role != Role::Assistant {
                            return Err(pairing_error(msg_idx, format_args!("tool_use {id:?} must have assistant role")));
                        }
                        if !pending.insert(id.as_str()) {
                            return Err(pairing_error(msg_idx, format_args!("duplicate tool_use {id:?}")));
                        }
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        if message.role != Role::User {
                            return Err(pairing_error(msg_idx, format_args!("tool_result {tool_use_id:?} must have user role")));
                        }
                        if saw_content {
                            return Err(pairing_error(msg_idx, format_args!("tool_result {tool_use_id:?} must precede ordinary content")));
                        }
                        if !answered.insert(tool_use_id.as_str()) {
                            return Err(pairing_error(msg_idx, format_args!("duplicate tool_result {tool_use_id:?}")));
                        }
                        if !unanswered.remove(tool_use_id.as_str()) {
                            return Err(pairing_error(msg_idx, format_args!("tool_result {tool_use_id:?} has no tool_use in the preceding assistant message")));
                        }
                    }
                    _ => saw_content = true,
                }
            }
        }
        if !unanswered.is_empty() {
            return Err(pairing_error(msg_idx - 1, format_args!("tool_uses {unanswered:?} lack adjacent tool_results")));
        }
    }
    if !pending.is_empty() {
        return Err(pairing_error(messages.len() - 1, format_args!("tool_uses {pending:?} lack adjacent tool_results")));
    }
    Ok(())
}

#[cfg(test)]
mod pairing_repair_tests {
    use super::*;

    fn assistant_with_tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({}),
            }]),
        }
    }

    fn results(ids: &[&str]) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Blocks(
                ids.iter()
                    .map(|id| ContentBlock::ToolResult {
                        tool_use_id: (*id).to_string(),
                        content: "ok".into(),
                        is_error: false,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn partial_results_precede_interleaved_content() {
        let mut calls = assistant_with_tool_use("call_a");
        let MessageContent::Blocks(ref mut blocks) = calls.content else { unreachable!() };
        let MessageContent::Blocks(extra) = assistant_with_tool_use("call_b").content else { unreachable!() };
        blocks.extend(extra);
        let mut reply = results(&["call_a", "ghost"]);
        let MessageContent::Blocks(ref mut blocks) = reply.content else { unreachable!() };
        blocks.insert(0, ContentBlock::Text { text: "peer update".into() });
        let mut state = HydrationState::new();
        state.messages = vec![calls, reply, assistant_with_tool_use("call_c"), results(&["call_c"])];

        let out = state.into_messages();
        assert_well_paired(&out);
        let MessageContent::Blocks(blocks) = &out[1].content else { panic!("expected results") };
        assert!(matches!(&blocks[0], ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "call_a"));
        assert!(matches!(&blocks[1], ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "call_b"));
        assert!(matches!(&blocks[2], ContentBlock::Text { text } if text == "peer update"));
        assert_eq!(blocks.len(), 3, "only the orphan is removed");
    }

    #[tokio::test]
    async fn provider_refuses_unpaired_history_before_dispatch() {
        use super::super::{BuildOpts, LlmError, MockClient, Provider};

        let mut duplicate_call = assistant_with_tool_use("duplicate_call");
        let MessageContent::Blocks(ref mut blocks) = duplicate_call.content else { unreachable!() };
        blocks.push(blocks[0].clone());
        let mut text_first = results(&["text_first"]);
        let MessageContent::Blocks(ref mut blocks) = text_first.content else { unreachable!() };
        blocks.insert(0, ContentBlock::Text { text: "peer update".into() });
        let malformed = [
            ("missing", vec![assistant_with_tool_use("missing")]),
            ("displaced", vec![assistant_with_tool_use("displaced"), Message::user("peer update"), results(&["displaced"])]),
            ("orphan", vec![results(&["orphan"])]),
            ("duplicate_result", vec![assistant_with_tool_use("duplicate_result"), results(&["duplicate_result", "duplicate_result"])]),
            ("wrong_role", vec![Message { role: Role::User, ..assistant_with_tool_use("wrong_role") }, results(&["wrong_role"])]),
            ("wrong_result_role", vec![assistant_with_tool_use("wrong_result_role"), Message { role: Role::Assistant, ..results(&["wrong_result_role"]) }]),
            ("duplicate_call", vec![duplicate_call, results(&["duplicate_call"])]),
            ("extra", vec![assistant_with_tool_use("call_a"), results(&["call_a", "extra"])]),
            ("text_first", vec![assistant_with_tool_use("text_first"), text_first]),
        ];
        for (id, messages) in malformed {
            let mock = MockClient::new("").with_scripted_stream(vec![vec![]]);
            let script = mock.scripted.as_ref().unwrap().clone();
            let provider = Provider::Mock(mock);
            let result = provider.stream(BuildOpts::new("mock"), messages).await;
            match result {
                Err(LlmError::InvalidRequest(detail)) => {
                    assert!(detail.contains(id), "error must name {id}: {detail}");
                    assert!(detail.contains("message"), "error must locate the defect: {detail}");
                }
                Err(error) => panic!("unexpected error: {error}"),
                Ok(_) => panic!("provider accepted malformed pairing for {id}"),
            }
            assert_eq!(script.lock().len(), 1, "refusal must precede backend dispatch");
        }
    }

    #[tokio::test]
    async fn provider_accepts_repaired_history() {
        use super::super::{BuildOpts, MockClient, Provider};

        let mut state = HydrationState::new();
        state.messages = vec![
            Message::user("go"),
            assistant_with_tool_use("interrupted"),
            Message::user("peer update"),
            results(&["interrupted"]),
            results(&["ghost"]),
            assistant_with_tool_use("next"),
            results(&["next"]),
        ];
        let out = state.into_messages();
        assert_well_paired(&out);
        let mock = MockClient::new("").with_scripted_stream(vec![vec![]]);
        let script = mock.scripted.as_ref().unwrap().clone();
        let provider = Provider::Mock(mock);
        assert!(provider.stream(BuildOpts::new("mock"), out).await.is_ok());
        assert!(script.lock().is_empty(), "valid pairing must reach the backend");
    }

    #[test]
    fn interleaved_block_fixture_matches_bootstrap_and_incremental_snapshots() {
        use kaijutsu_types::{ContextId, PrincipalId, ToolKind};
        use super::super::{ConversationMailbox, hydrate_from_blocks};

        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let id = |seq| BlockId::new(ctx, principal, seq);
        let call = |seq, tool_id: &str| BlockSnapshot::tool_call(
            id(seq), None, ToolKind::Shell, "shell", serde_json::json!({}),
            BlockRole::Model, Some(tool_id.into()),
        );
        let result = |seq, call_seq, tool_id: &str| BlockSnapshot::tool_result(
            id(seq), id(call_seq), ToolKind::Shell, "ok", false, Some(0), Some(tool_id.into()),
        );
        let blocks = vec![
            BlockSnapshot::text(id(0), None, BlockRole::User, "go"),
            call(1, "call_a"),
            call(2, "call_b"),
            result(3, 1, "call_a"),
            BlockSnapshot::text(id(4), None, BlockRole::User, "peer update"),
            result(5, 2, "call_b"),
            result(6, 99, "ghost"),
            call(7, "call_c"),
            result(8, 7, "call_c"),
        ];
        let expected = hydrate_from_blocks(&blocks);
        assert_well_paired(&expected);
        let mut mailbox = ConversationMailbox::new();
        for block in &blocks {
            mailbox.feed(block, None);
            let snapshot = mailbox.snapshot();
            assert_well_paired(&snapshot);
            assert_eq!(serde_json::to_value(&snapshot).unwrap(), serde_json::to_value(mailbox.snapshot()).unwrap());
        }
        assert_eq!(serde_json::to_value(mailbox.snapshot()).unwrap(), serde_json::to_value(&expected).unwrap());
        let answered: Vec<_> = expected.iter().flat_map(|message| match &message.content {
            MessageContent::Blocks(blocks) => blocks.iter().filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, is_error, .. } => Some((tool_use_id.as_str(), *is_error)),
                _ => None,
            }).collect::<Vec<_>>(),
            _ => vec![],
        }).collect();
        assert_eq!(answered, vec![("call_a", false), ("call_b", true), ("call_c", false)]);
        assert!(expected.iter().any(|message| message.as_text() == Some("peer update")));
    }

    fn call_ids(message: &Message) -> Vec<&str> {
        let MessageContent::Blocks(blocks) = &message.content else { return vec![] };
        blocks.iter().filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        }).collect()
    }

    fn result_ids(message: &Message) -> Vec<&str> {
        let MessageContent::Blocks(blocks) = &message.content else { return vec![] };
        blocks.iter().filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        }).collect()
    }

    /// Check both directions independently of the production validator.
    fn assert_well_paired(messages: &[Message]) {
        for (idx, message) in messages.iter().enumerate() {
            let mut calls = call_ids(message);
            let mut results = result_ids(message);
            match message.role {
                Role::Assistant => {
                    assert!(results.is_empty(), "assistant results at {idx}");
                    if calls.is_empty() { continue; }
                    let next = messages.get(idx + 1).expect("a call batch must have a reply");
                    assert_eq!(next.role, Role::User);
                    let mut answers = result_ids(next);
                    calls.sort();
                    answers.sort();
                    assert_eq!(calls, answers, "call batch at {idx} must be answered exactly");
                    calls.dedup();
                    assert_eq!(calls.len(), answers.len(), "duplicate call ids at {idx}");
                }
                Role::User => {
                    assert!(calls.is_empty(), "user tool calls at {idx}");
                    if results.is_empty() { continue; }
                    assert!(idx > 0, "orphan results at start");
                    assert_eq!(messages[idx - 1].role, Role::Assistant);
                    let mut expected = call_ids(&messages[idx - 1]);
                    results.sort();
                    expected.sort();
                    assert_eq!(results, expected, "orphan results at {idx}");
                    let MessageContent::Blocks(blocks) = &message.content else { unreachable!() };
                    assert!(blocks[..results.len()].iter().all(|b| matches!(b, ContentBlock::ToolResult { .. })), "results must lead at {idx}");
                }
            }
        }
    }

    #[test]
    fn repair_is_idempotent_for_short_interleavings() {
        let choices = [
            Message::user("peer update"), Message::assistant("thinking"),
            assistant_with_tool_use("a"), assistant_with_tool_use("b"),
            results(&["a"]), results(&["b"]), results(&["ghost"]),
        ];
        for mut shape in 0..choices.len().pow(4) {
            let mut state = HydrationState::new();
            for _ in 0..4 {
                state.messages.push(choices[shape % choices.len()].clone());
                shape /= choices.len();
            }
            let repaired = state.into_messages();
            assert_well_paired(&repaired);
            assert!(validate_tool_pairing(&repaired).is_ok());
            let mut again = HydrationState::new();
            again.messages = repaired.clone();
            assert_eq!(serde_json::to_value(again.into_messages()).unwrap(), serde_json::to_value(repaired).unwrap());
        }
    }

    #[test]
    fn repair_preserves_ambiguous_duplicates_for_refusal() {
        let mut duplicate_calls = assistant_with_tool_use("duplicate");
        let MessageContent::Blocks(ref mut blocks) = duplicate_calls.content else { unreachable!() };
        blocks.push(ContentBlock::ToolUse {
            id: "duplicate".into(), name: "different_tool".into(), input: serde_json::json!({}),
        });
        let mut duplicate_results = results(&["duplicate"]);
        let MessageContent::Blocks(ref mut blocks) = duplicate_results.content else { unreachable!() };
        blocks.push(ContentBlock::ToolResult {
            tool_use_id: "duplicate".into(), content: "conflicting output".into(), is_error: true,
        });
        for messages in [
            vec![duplicate_calls, results(&["duplicate"])],
            vec![assistant_with_tool_use("duplicate"), duplicate_results],
        ] {
            let expected = serde_json::to_value(&messages).unwrap();
            let mut state = HydrationState::new();
            state.messages = messages;
            let repaired = state.into_messages();
            assert_eq!(serde_json::to_value(&repaired).unwrap(), expected, "repair must not choose a winner");
            assert!(matches!(validate_tool_pairing(&repaired), Err(LlmError::InvalidRequest(_))));
        }
    }

    #[test]
    fn a_dropped_orphan_does_not_desync_the_pairs_after_it() {
        let mut state = HydrationState::new();
        state.messages = vec![
            Message::user("go"),
            assistant_with_tool_use("call_a"),
            results(&["call_a"]),
            // A late result for a call that is nowhere in this conversation:
            // the whole message is orphaned and gets dropped.
            results(&["call_vanished"]),
            // Everything after here was collateral damage.
            assistant_with_tool_use("call_b"),
            results(&["call_b"]),
            assistant_with_tool_use("call_c"),
            results(&["call_c"]),
        ];

        let out = state.into_messages();

        assert_well_paired(&out);
        let ids: Vec<&str> = out
            .iter()
            .flat_map(|m| match &m.content {
                MessageContent::Blocks(bs) => bs
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        assert!(
            ids.contains(&"call_b") && ids.contains(&"call_c"),
            "results after a dropped orphan must survive; got {ids:?}"
        );
        assert!(
            !ids.contains(&"call_vanished"),
            "the genuinely orphaned result must still be dropped; got {ids:?}"
        );
    }

    #[test]
    fn several_dropped_orphans_still_leave_later_pairs_intact() {
        let mut state = HydrationState::new();
        state.messages = vec![
            Message::user("go"),
            results(&["ghost_one"]),
            results(&["ghost_two"]),
            assistant_with_tool_use("call_real"),
            results(&["call_real"]),
        ];

        let out = state.into_messages();
        assert_well_paired(&out);
    }
}

#[cfg(test)]
mod estimate_tokens_tests {
    use super::*;

    #[test]
    fn empty_messages_is_zero() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn text_message_counts_overhead_plus_bytes_over_four() {
        // "hello" = 5 bytes -> 5/4 = 1 token (integer division), plus the
        // fixed per-message overhead.
        let messages = vec![Message::user("hello")];
        assert_eq!(
            estimate_tokens(&messages),
            PER_MESSAGE_TOKEN_OVERHEAD + 1,
            "5-byte text should contribute 5/4=1 token plus per-message overhead"
        );
    }

    #[test]
    fn longer_text_scales_with_byte_count() {
        let text: String = "x".repeat(400); // 400 bytes -> 100 tokens
        let messages = vec![Message::user(text)];
        assert_eq!(estimate_tokens(&messages), PER_MESSAGE_TOKEN_OVERHEAD + 100);
    }

    #[test]
    fn image_block_uses_flat_constant_regardless_of_resolution() {
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![ContentBlock::Image {
                hash: "deadbeef".to_string(),
                media_type: "image/png".to_string(),
                data_base64: None, // unresolved, as it always is at hydration time
            }]),
        }];
        assert_eq!(
            estimate_tokens(&messages),
            PER_MESSAGE_TOKEN_OVERHEAD + ESTIMATED_TOKENS_PER_IMAGE
        );
    }

    #[test]
    fn blocks_mix_sums_text_tool_use_tool_result_and_image() {
        let tool_input = serde_json::json!({"path": "/tmp/foo.txt"});
        let input_str = serde_json::to_string(&tool_input).unwrap();
        let expected_tool_use = bytes_to_tokens("read_file".len() + input_str.len());
        let expected_text = bytes_to_tokens("some assistant text".len());
        let expected_tool_result = bytes_to_tokens("file contents here".len());

        let messages = vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "some assistant text".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tool_1".to_string(),
                    name: "read_file".to_string(),
                    input: tool_input,
                },
                ContentBlock::Image {
                    hash: "abc123".to_string(),
                    media_type: "image/jpeg".to_string(),
                    data_base64: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tool_1".to_string(),
                    content: "file contents here".to_string(),
                    is_error: false,
                },
            ]),
        }];

        let expected = PER_MESSAGE_TOKEN_OVERHEAD
            + expected_text
            + expected_tool_use
            + ESTIMATED_TOKENS_PER_IMAGE
            + expected_tool_result;
        assert_eq!(estimate_tokens(&messages), expected);
    }

    #[test]
    fn reasoning_block_counts_text_bytes() {
        let text = "x".repeat(40); // 40 bytes -> 10 tokens
        let messages = vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Reasoning {
                text,
                signature: Some("sig".to_string()),
            }]),
        }];
        assert_eq!(estimate_tokens(&messages), PER_MESSAGE_TOKEN_OVERHEAD + 10);
    }

    #[test]
    fn multiple_messages_sum_independently() {
        let messages = vec![
            Message::user("x".repeat(40)), // 10 tokens + overhead
            Message::assistant("y".repeat(80)), // 20 tokens + overhead
        ];
        assert_eq!(
            estimate_tokens(&messages),
            2 * PER_MESSAGE_TOKEN_OVERHEAD + 10 + 20
        );
    }
}

/// Hydration is blind to styling, by construction (docs/ansi-and-beyond.md).
///
/// `style_spans` and `provenance` describe how a block LOOKS and where its
/// content came from; the model reads the stripped content and nothing else.
/// The blindness is currently a property of `translate_block` reading only
/// `content` / `stderr` / the `format_*` helpers — nothing forbids a future
/// edit from reaching for a span, so this pins the contract rather than
/// trusting the omission to stay.
#[cfg(test)]
mod hydration_blindness_tests {
    use super::*;
    use kaijutsu_types::{
        BlockSnapshotBuilder, ProvenanceTag, StyleAttrs, StyleColor, StyleSpan,
    };

    fn spans() -> Vec<StyleSpan> {
        vec![
            StyleSpan {
                start: 0,
                end: 5,
                fg: Some(StyleColor::Indexed(9)),
                bg: None,
                attrs: StyleAttrs::BOLD | StyleAttrs::UNDERLINE,
            },
            StyleSpan {
                start: 5,
                end: 11,
                fg: Some(StyleColor::Rgb(0x33, 0xFF, 0x99)),
                bg: Some(StyleColor::Indexed(0)),
                attrs: StyleAttrs::default(),
            },
        ]
    }

    fn hydrate(block: &BlockSnapshot) -> String {
        let mut state = HydrationState::new();
        state.translate_block(block, None);
        serde_json::to_string(&state.into_messages()).expect("messages serialize")
    }

    #[test]
    fn spans_and_provenance_never_reach_the_model() {
        let ctx = kaijutsu_types::ContextId::new();
        let principal = kaijutsu_types::PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            principal_id: principal,
            seq: 1,
        };

        for kind in [BlockKind::Text, BlockKind::ToolResult] {
            let plain = BlockSnapshotBuilder::new(id, kind)
                .role(BlockRole::User)
                .content("hello world")
                .build();
            let styled = BlockSnapshotBuilder::new(id, kind)
                .role(BlockRole::User)
                .content("hello world")
                .style_spans(spans())
                .provenance(ProvenanceTag {
                    transform: "ansi-strip".into(),
                    version: 1,
                })
                .build();

            assert_eq!(
                hydrate(&styled),
                hydrate(&plain),
                "{kind:?}: a spanned block must hydrate to exactly the same \
                 messages as the same block without spans"
            );
        }
    }
}
