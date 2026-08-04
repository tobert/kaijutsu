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
//! - **Incremental** *(future)*. The per-context mailbox subscriber
//!   feeds blocks one at a time as they're inserted, keeping the live
//!   session in sync without rebuilding from scratch each turn.
//!
//! Both paths share the same `translate_block` / `into_messages` pair
//! so the wire-history contract stays identical.

use std::collections::HashMap;

use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContentType, Role as BlockRole};

use super::{ContentBlock, Message, MessageContent, Role};

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
        // Skip blocks that shouldn't appear in LLM history
        if block.ephemeral {
            return;
        }
        if block.excluded {
            return;
        }
        if matches!(block.kind, BlockKind::File | BlockKind::Trace) {
            return;
        }
        // Skip System blocks unless they're Drift, Error, Notification,
        // Resource, or Task (D-34; Task added for household-agent grooming —
        // docs/tasks.md). Task's builder/constructor don't force `role =
        // System` the way Notification/Resource/Error do (a task follows
        // ordinary content-authorship), but this stays in the allow-list
        // defensively in case a future producer (e.g. an rc-seeded default
        // task list) creates one as System.
        if block.role == BlockRole::System
            && block.kind != BlockKind::Drift
            && block.kind != BlockKind::Error
            && block.kind != BlockKind::Notification
            && block.kind != BlockKind::Resource
            && block.kind != BlockKind::Task
        {
            return;
        }
        if block.content.is_empty()
            && block.kind != BlockKind::ToolCall
            && block.kind != BlockKind::ToolResult
            && block.kind != BlockKind::Error
            && block.kind != BlockKind::Notification
            && block.kind != BlockKind::Resource
            && block.kind != BlockKind::Task
        {
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

    /// Consume the state and emit the final message sequence, repairing
    /// tool_use/tool_result pairing.
    ///
    /// The LLM API requires that every assistant message containing
    /// `tool_use` blocks is immediately followed by a user message with
    /// matching `tool_result` blocks for **each** tool_use id, and
    /// conversely that tool_result blocks only appear after an assistant
    /// message containing the matching tool_use.
    ///
    /// Forks, interrupts, and out-of-order tool execution can break both
    /// directions:
    /// - **Orphaned tool_uses**: synthesize `is_error: true` results.
    /// - **Late tool_results**: drop results whose tool_use already has
    ///   a (synthetic or real) result earlier in the conversation.
    pub(crate) fn into_messages(mut self) -> Vec<Message> {
        self.flush_all();

        // ── Pass 1: Forward repair (orphaned tool_uses → synthetic results) ──
        let mut repaired: Vec<Message> = Vec::with_capacity(self.messages.len() + 4);
        let len = self.messages.len();
        let mut i = 0;

        while i < len {
            let msg = &self.messages[i];

            // Extract tool_use ids from this assistant message (if any).
            let tool_use_ids: Vec<String> = if msg.role == Role::Assistant {
                if let MessageContent::Blocks(blocks) = &msg.content {
                    blocks
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::ToolUse { id, .. } = b {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            repaired.push(msg.clone());

            if tool_use_ids.is_empty() {
                i += 1;
                continue;
            }

            // Collect tool_result ids already present in the next message.
            let covered: std::collections::HashSet<&str> = self
                .messages
                .get(i + 1)
                .and_then(|next| {
                    if next.role != Role::User {
                        return None;
                    }
                    if let MessageContent::Blocks(blocks) = &next.content {
                        Some(
                            blocks
                                .iter()
                                .filter_map(|b| {
                                    if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                                        Some(tool_use_id.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                        )
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let missing: Vec<String> = tool_use_ids
                .into_iter()
                .filter(|id| !covered.contains(id.as_str()))
                .collect();

            if missing.is_empty() {
                i += 1;
                continue;
            }

            tracing::warn!(
                msg_idx = i,
                ?missing,
                covered_count = covered.len(),
                "hydration repair: synthesizing tool_results for orphaned tool_uses"
            );

            let error_results: Vec<ContentBlock> = missing
                .into_iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: "Tool execution was interrupted (context was forked or pruned)"
                        .into(),
                    is_error: true,
                })
                .collect();

            if covered.is_empty() {
                // No tool_result message follows at all — insert one.
                repaired.push(Message::tool_results(error_results));
            } else {
                // Next message has *some* results — append the missing ones
                // into it so all results stay in one user message.
                i += 1;
                let mut next = self.messages[i].clone();
                if let MessageContent::Blocks(ref mut blocks) = next.content {
                    blocks.extend(error_results);
                }
                repaired.push(next);
            }

            i += 1;
        }

        // ── Pass 2: Reverse repair (orphaned tool_results → drop) ──
        // Late-arriving ToolResult blocks that already have a synthetic
        // error result produce User messages with tool_results that don't
        // match any tool_use in the preceding assistant message. The API
        // rejects these. Strip them out.
        let mut cleaned: Vec<Message> = Vec::with_capacity(repaired.len());
        for (idx, msg) in repaired.iter().enumerate() {
            if msg.role == Role::User
                && let MessageContent::Blocks(blocks) = &msg.content
            {
                // Get tool_use IDs from the preceding assistant message
                let preceding_tool_uses: std::collections::HashSet<&str> = idx
                    .checked_sub(1)
                    .and_then(|prev_idx| cleaned.get(prev_idx))
                    .and_then(|prev| {
                        if prev.role != Role::Assistant {
                            return None;
                        }
                        if let MessageContent::Blocks(pblocks) = &prev.content {
                            Some(
                                pblocks
                                    .iter()
                                    .filter_map(|b| {
                                        if let ContentBlock::ToolUse { id, .. } = b {
                                            Some(id.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                // Filter: keep only tool_results that match a preceding tool_use,
                // plus any non-tool-result blocks (text).
                let filtered: Vec<ContentBlock> = blocks
                    .iter()
                    .filter(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => {
                            if preceding_tool_uses.contains(tool_use_id.as_str()) {
                                true
                            } else {
                                tracing::warn!(
                                    msg_idx = idx,
                                    tool_use_id,
                                    "hydration repair: dropping orphaned tool_result (late arrival)"
                                );
                                false
                            }
                        }
                        _ => true,
                    })
                    .cloned()
                    .collect();

                if filtered.is_empty() {
                    // Entire message was orphaned tool_results — skip it
                    continue;
                }
                if filtered.len() < blocks.len() {
                    // Some blocks were dropped — push the filtered version
                    cleaned.push(Message {
                        role: Role::User,
                        content: MessageContent::Blocks(filtered),
                    });
                    continue;
                }
            }
            cleaned.push(msg.clone());
        }

        cleaned
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
