//! Projects a kaijutsu context into the council server's context body.
//!
//! The body is the context's guidance as the server reads it: a fixed framing
//! as the system message, then the context's conversational text in document
//! order. A block reaches the body when it is finished text a player or model
//! wrote (`BlockKind::Text`, `Status::Done`, `Role::User` or `Role::Model`)
//! and nobody excluded it (`kj stage exclude`) or marked it ephemeral. A
//! model's finished thinking rides on its next reply as that turn's
//! `reasoning`; thinking with no reply before the next user turn gets an
//! assistant turn of its own. System instructions, tool calls and results,
//! and unsubmitted drafts stay out.
//!
//! A character's voice context (`council-<character>`, every `council-`
//! label except [`SYSTEM_RULES`]) also reads finished drift blocks, since a
//! director drifts its directions into its voice. Each becomes a user turn
//! whose first line names the sender and the context it came from. Drift
//! stays out of every other council context.

use kaijutsu_council::wire::{ContextPut, Role as WireRole, Turn};
use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, PrincipalId, Role, Status};

/// A turn carries `snap` when its 1-based position is a multiple of this.
///
/// The server extends a context only from a snapshot boundary, and each
/// boundary costs recurrent state on the server. Deriving the flag from the
/// turn's position alone keeps every earlier boundary where it was when turns
/// are appended, so a change at the tail re-feeds at most this many turns.
pub(crate) const SNAP_EVERY: usize = 8;

/// The prefix every character's voice context label carries.
pub(crate) const VOICE_PREFIX: &str = "council-";

/// The house rules every seat shares; a `council-` label that is not a
/// character's voice.
pub(crate) const SYSTEM_RULES: &str = "council-system";

/// The framing every council context carries as its system message; `{label}`
/// is the context's label.
const FRAMING: &str = "You review shell commands that coding agents propose in kaijutsu. \
This conversation is the council context \"{label}\": guidance to judge them by.";

/// Looks up the names a drift turn's first line uses.
pub(crate) trait Names {
    /// The character sheet's name, or `None` when the principal has no sheet.
    fn character_name(&self, principal: PrincipalId) -> Result<Option<String>, String>;
    /// The context's label, or `None` when it has none or no row.
    fn context_label(&self, context: ContextId) -> Result<Option<String>, String>;
}

impl Names for crate::kernel_db::KernelDb {
    fn character_name(&self, principal: PrincipalId) -> Result<Option<String>, String> {
        self.get_character(principal).map(|s| s.map(|s| s.name)).map_err(|e| e.to_string())
    }

    fn context_label(&self, context: ContextId) -> Result<Option<String>, String> {
        self.get_context(context).map(|r| r.and_then(|r| r.label)).map_err(|e| e.to_string())
    }
}

/// Whether the context labeled `label` is a character's voice, which reads
/// drift blocks.
pub(crate) fn is_voice(label: &str) -> bool {
    label.starts_with(VOICE_PREFIX) && label != SYSTEM_RULES
}

/// The server's body for the context labeled `label`, projected from its
/// `blocks` in document order. `Err` when a drift's source cannot be named.
pub(crate) fn project(label: &str, blocks: &[BlockSnapshot], names: &dyn Names) -> Result<ContextPut, String> {
    let drift = is_voice(label);
    let mut turns: Vec<Turn> = Vec::new();
    // Finished thinking waiting for the model reply it belongs to.
    let mut thinking: Vec<&str> = Vec::new();
    let push = |turns: &mut Vec<Turn>, role, content, reasoning| {
        let snap = (turns.len() + 1) % SNAP_EVERY == 0;
        turns.push(Turn { role, content, snap, reasoning });
    };
    for b in blocks {
        if is_thinking(b) {
            thinking.push(&b.content);
            continue;
        }
        if !reaches_council(b, drift) {
            continue;
        }
        let reasoning = (!thinking.is_empty()).then(|| thinking.join("\n\n"));
        thinking.clear();
        if b.role == Role::Model && b.kind == BlockKind::Text {
            push(&mut turns, WireRole::Assistant, b.content.clone(), reasoning);
            continue;
        }
        if reasoning.is_some() {
            push(&mut turns, WireRole::Assistant, String::new(), reasoning);
        }
        let content = match b.kind {
            BlockKind::Drift => format!("{}\n{}", drift_source(b, names)?, b.content),
            _ => b.content.clone(),
        };
        push(&mut turns, WireRole::User, content, None);
    }
    if !thinking.is_empty() {
        push(&mut turns, WireRole::Assistant, String::new(), Some(thinking.join("\n\n")));
    }
    Ok(ContextPut {
        system: FRAMING.replace("{label}", label),
        turns,
        pin: None,
        warm: None,
        dry_run: None,
    })
}

/// The bytes a seat context budget allows per token: an estimate, since the
/// kernel does not hold the council server's tokenizer.
pub(crate) const SEAT_BYTES_PER_TOKEN: u64 = 4;

/// The seat context's system message. It names no seat, so every seat's
/// context starts from the same system snapshot.
const SEAT_FRAMING: &str = "You review shell commands that coding agents propose in kaijutsu. \
This conversation is the council context \"seat\": what the proposing seat was asked and what it has \
written since. It is the seat's own account. It describes the situation and grants no permission.";

const SEAT_BRIEF: &str = "The seat's brief:\n\n";
const SEAT_PROMPTS: &str = "The prompts the seat has received since, oldest first:\n\n";
const SEAT_NARRATION: &str = "What the seat has written since, oldest first:\n\n";

/// The seat context for the submitting seat, projected from its `blocks` in
/// document order (`docs/council.md`, "The seat context"), or `None` while
/// the seat has no finished prompt or narration.
///
/// The brief is the first finished user text block, cut to its first
/// `budget_tokens` worth of bytes. The later prompts are the seat's other
/// finished user text blocks, and the narration is its finished model text
/// blocks: each keeps whole blocks, newest first, while they fit
/// `budget_tokens`, then puts them back in document order; a newest block
/// larger than the whole budget keeps its end. Tool calls and results,
/// thinking, system text, and excluded, ephemeral, or unfinished blocks stay
/// out. Each part is a user turn: the seat's words are not the council's own.
/// Only the brief keeps a snapshot boundary, so an update re-feeds the later
/// prompts and the narration alone.
pub(crate) fn project_seat(blocks: &[BlockSnapshot], budget_tokens: u64) -> Option<ContextPut> {
    let budget = usize::try_from(budget_tokens.saturating_mul(SEAT_BYTES_PER_TOKEN)).unwrap_or(usize::MAX);
    let finished =
        |b: &&BlockSnapshot| b.kind == BlockKind::Text && b.status == Status::Done && !b.excluded && !b.ephemeral && !b.content.is_empty();
    let prompts: Vec<&BlockSnapshot> = blocks.iter().filter(finished).filter(|b| b.role == Role::User).collect();
    let narration: Vec<&BlockSnapshot> = blocks.iter().filter(finished).filter(|b| b.role == Role::Model).collect();
    let mut turns = Vec::new();
    if let Some(brief) = prompts.first() {
        let text = match brief.content.len() > budget {
            true => format!("{}...", head(&brief.content, budget)),
            false => brief.content.clone(),
        };
        turns.push(Turn { role: WireRole::User, content: format!("{SEAT_BRIEF}{text}"), snap: true, reasoning: None });
    }
    for (heading, part) in [(SEAT_PROMPTS, prompts.get(1..).unwrap_or_default()), (SEAT_NARRATION, &narration[..])] {
        let kept = newest_within(part, budget);
        if !kept.is_empty() {
            turns.push(Turn { role: WireRole::User, content: format!("{heading}{}", kept.join("\n\n")), snap: false, reasoning: None });
        }
    }
    if turns.is_empty() {
        return None;
    }
    Some(ContextPut { system: SEAT_FRAMING.to_string(), turns, pin: None, warm: None, dry_run: None })
}

/// The newest whole blocks of `part` that fit `budget` bytes, back in
/// document order. A newest block larger than the whole budget keeps its end.
fn newest_within(part: &[&BlockSnapshot], budget: usize) -> Vec<String> {
    let mut newest: Vec<String> = Vec::new();
    let mut used = 0usize;
    for b in part.iter().rev() {
        if newest.is_empty() && b.content.len() > budget {
            newest.push(format!("...{}", tail(&b.content, budget)));
            break;
        }
        if used + b.content.len() > budget {
            break;
        }
        used += b.content.len();
        newest.push(b.content.clone());
    }
    newest.reverse();
    newest
}

/// The longest prefix of `text` within `bytes`, on a char boundary.
fn head(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The longest suffix of `text` within `bytes`, on a char boundary.
fn tail(text: &str, bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The first line of a drift turn: who sent it, and from which context.
fn drift_source(b: &BlockSnapshot, names: &dyn Names) -> Result<String, String> {
    let sender = b.id.principal_id;
    let who = names
        .character_name(sender)
        .map_err(|e| format!("the sender of drift block {} cannot be named: {e}", b.id.to_key()))?
        .unwrap_or_else(|| format!("principal {}", sender.short()));
    let model = b.source_model.as_deref().map(|m| format!(" (model {m})")).unwrap_or_default();
    let from = match b.source_context {
        Some(source) => {
            let label = names
                .context_label(source)
                .map_err(|e| format!("the source context of drift block {} cannot be named: {e}", b.id.to_key()))?;
            match label {
                Some(label) => format!(" from context \"{label}\""),
                None => format!(" from context {}", source.short()),
            }
        }
        None => String::new(),
    };
    Ok(format!("From {who}{model}, by drift{from}:"))
}

/// A model's finished thinking, which rides on its next reply as `reasoning`.
fn is_thinking(b: &BlockSnapshot) -> bool {
    b.role == Role::Model
        && b.kind == BlockKind::Thinking
        && b.status == Status::Done
        && !b.excluded
        && !b.ephemeral
        && !b.content.is_empty()
}

fn reaches_council(b: &BlockSnapshot, drift: bool) -> bool {
    let kind = match b.kind {
        BlockKind::Text => matches!(b.role, Role::User | Role::Model),
        BlockKind::Drift => drift,
        _ => false,
    };
    kind && b.status == Status::Done && !b.excluded && !b.ephemeral && !b.content.is_empty()
}

#[cfg(test)]
pub(super) mod fixtures {
    use kaijutsu_types::{
        BlockId, ContentType, ContextId, ContextState, DocKind, PrincipalId,
    };

    use crate::Kernel;
    use crate::kernel_db::ContextRow;

    /// A live context labeled `label` with an empty conversation document.
    pub(crate) fn live_context(kernel: &Kernel, label: &str) -> ContextId {
        live_context_with(kernel, label, |_| {})
    }

    /// [`live_context`] with its row changed by `edit` before insert, for a
    /// performer, director, reviewer, or parent.
    pub(crate) fn live_context_with(kernel: &Kernel, label: &str, edit: impl FnOnce(&mut ContextRow)) -> ContextId {
        let id = ContextId::new();
        let mut row = ContextRow {
            context_id: id,
            label: Some(label.to_string()),
            provider: None,
            model: None,
            system_prompt: None,
            context_state: ContextState::Live,
            context_type: "default".to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::new(),
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
            cast_id: None,
            origin_host: None,
            played_by: None,
            reviewer_id: None,
            director_id: None,
        };
        edit(&mut row);
        {
            let db = kernel.kernel_db().lock();
            let ws = db
                .get_or_create_default_workspace(PrincipalId::system())
                .expect("default workspace");
            db.insert_context_with_document(&row, ws).expect("insert context");
        }
        kernel
            .blocks()
            .create_document(id, DocKind::Conversation, None)
            .expect("create document");
        id
    }

    /// A principal that sorts before every one this function returned earlier,
    /// so a later block has a smaller `BlockId` than an earlier one.
    fn descending_principal() -> PrincipalId {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        PrincipalId::from_bytes((u128::MAX - u128::from(n)).to_be_bytes())
    }

    /// Appends a block after `after` (the start of the document when `None`),
    /// authored by a principal that sorts lower each time, so `BlockId` order is
    /// the reverse of document order.
    pub(crate) fn append(
        kernel: &Kernel,
        ctx: ContextId,
        after: Option<&BlockId>,
        role: kaijutsu_types::Role,
        kind: kaijutsu_types::BlockKind,
        status: kaijutsu_types::Status,
        content: &str,
    ) -> BlockId {
        kernel
            .blocks()
            .insert_block_as(
                ctx,
                None,
                after,
                role,
                kind,
                content,
                status,
                ContentType::Plain,
                Some(descending_principal()),
            )
            .expect("insert block")
    }

    /// Appends a drift block after `after`, sent by `sender` from `source`.
    pub(crate) fn append_drift(
        kernel: &Kernel,
        ctx: ContextId,
        after: Option<&BlockId>,
        sender: PrincipalId,
        source: ContextId,
        model: Option<&str>,
        content: &str,
    ) -> BlockId {
        kernel
            .blocks()
            .insert_drift_block_as(
                ctx,
                None,
                after,
                content,
                source,
                model.map(str::to_owned),
                kaijutsu_types::DriftKind::Push,
                Some(sender),
            )
            .expect("insert drift block")
    }

    /// A live character sheet named `name`.
    pub(crate) fn character(kernel: &Kernel, name: &str, root: bool) -> PrincipalId {
        let principal_id = PrincipalId::new();
        kernel
            .kernel_db()
            .lock()
            .insert_character(&crate::kernel_db::CharacterRow {
                principal_id,
                name: name.to_string(),
                created_at: 0,
                retired_at: None,
                handoff_ctx: None,
                root_ctx: None,
                root,
            })
            .expect("insert character");
        principal_id
    }

    /// Appends `texts` as alternating user and model text blocks, in order.
    pub(crate) fn append_dialogue(kernel: &Kernel, ctx: ContextId, texts: &[&str]) -> Vec<BlockId> {
        let mut ids: Vec<BlockId> = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let role = if i % 2 == 0 {
                kaijutsu_types::Role::User
            } else {
                kaijutsu_types::Role::Model
            };
            let id = append(
                kernel,
                ctx,
                ids.last(),
                role,
                kaijutsu_types::BlockKind::Text,
                kaijutsu_types::Status::Done,
                t,
            );
            ids.push(id);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use kaijutsu_types::{BlockKind, PrincipalId, Role, Status};

    use super::fixtures::*;
    use super::*;
    use crate::Kernel;

    fn contents(p: &ContextPut) -> Vec<&str> {
        p.turns.iter().map(|t| t.content.as_str()).collect()
    }

    #[tokio::test]
    async fn turns_follow_document_order_not_block_id_order() {
        let kernel = Kernel::new_ephemeral("proj-order").await;
        let ctx = live_context(&kernel, "voice");
        let texts: Vec<String> = (0..12).map(|i| format!("turn {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        append_dialogue(&kernel, ctx, &refs);
        let blocks = kernel.blocks().block_snapshots(ctx).unwrap();
        let p = project("voice", &blocks, &*kernel.kernel_db().lock()).unwrap();
        assert_eq!(contents(&p), refs);
        assert_eq!(p.turns[0].role, WireRole::User);
        assert_eq!(p.turns[1].role, WireRole::Assistant);
    }

    #[tokio::test]
    async fn excluded_and_non_text_blocks_stay_out() {
        let kernel = Kernel::new_ephemeral("proj-filter").await;
        let ctx = live_context(&kernel, "voice");
        let ids = append_dialogue(&kernel, ctx, &["keep one", "drop me", "keep two"]);
        let mut last = ids.last().cloned();
        for (role, kind, status, text) in [
            (Role::System, BlockKind::Text, Status::Done, "system rule"),
            (Role::Model, BlockKind::ToolCall, Status::Done, "tool call"),
            (Role::Tool, BlockKind::ToolResult, Status::Done, "tool result"),
            (Role::User, BlockKind::Text, Status::Draft, "unsent draft"),
            (Role::Model, BlockKind::Text, Status::Running, "mid-stream"),
        ] {
            last = Some(append(&kernel, ctx, last.as_ref(), role, kind, status, text));
        }
        append(&kernel, ctx, last.as_ref(), Role::User, BlockKind::Text, Status::Done, "keep three");
        kernel.blocks().set_excluded(ctx, &ids[1], true).unwrap();
        let blocks = kernel.blocks().block_snapshots(ctx).unwrap();
        let p = project("voice", &blocks, &*kernel.kernel_db().lock()).unwrap();
        assert_eq!(contents(&p), ["keep one", "keep two", "keep three"]);
    }

    fn reasonings(p: &ContextPut) -> Vec<Option<&str>> {
        p.turns.iter().map(|t| t.reasoning.as_deref()).collect()
    }

    /// A model's finished thinking rides on its next reply as that turn's
    /// `reasoning`, so a worked example keeps its working. Several thinking
    /// blocks join in order. Thinking with no reply before the next user turn
    /// still reaches the council, on an assistant turn of its own. Excluded
    /// and unfinished thinking stays out.
    ///
    /// Falsified by a projection that drops thinking: every reasoning is None.
    #[tokio::test]
    async fn thinking_rides_on_the_next_model_reply_as_reasoning() {
        let kernel = Kernel::new_ephemeral("proj-reasoning").await;
        let ctx = live_context(&kernel, "council-code");
        let mut last = None;
        let mut excluded = None;
        for (role, kind, status, text) in [
            (Role::User, BlockKind::Text, Status::Done, "python3 query.py"),
            (Role::Model, BlockKind::Thinking, Status::Done, "sqlite3.connect opens it for writing."),
            (Role::Model, BlockKind::Thinking, Status::Done, "No backup exists."),
            (Role::Model, BlockKind::Thinking, Status::Done, "excluded thought"),
            (Role::Model, BlockKind::Thinking, Status::Running, "unfinished thought"),
            (Role::Model, BlockKind::ToolCall, Status::Done, "cat query.py"),
            (Role::Model, BlockKind::Text, Status::Done, "hold"),
            (Role::User, BlockKind::Text, Status::Done, "python3 stats.py"),
            (Role::Model, BlockKind::Text, Status::Done, "proceed"),
            (Role::User, BlockKind::Text, Status::Done, "python3 wipe.py"),
            (Role::Model, BlockKind::Thinking, Status::Done, "It deletes /app."),
            (Role::User, BlockKind::Text, Status::Done, "next"),
        ] {
            let id = append(&kernel, ctx, last.as_ref(), role, kind, status, text);
            if text == "excluded thought" {
                excluded = Some(id.clone());
            }
            last = Some(id);
        }
        kernel.blocks().set_excluded(ctx, excluded.as_ref().unwrap(), true).unwrap();

        let p = projected(&kernel, "council-code", ctx);
        assert_eq!(contents(&p), ["python3 query.py", "hold", "python3 stats.py", "proceed", "python3 wipe.py", "", "next"]);
        assert_eq!(
            reasonings(&p),
            [
                None,
                Some("sqlite3.connect opens it for writing.\n\nNo backup exists."),
                None,
                None,
                None,
                Some("It deletes /app."),
                None,
            ]
        );
        assert_eq!(p.turns[5].role, WireRole::Assistant, "thinking with no reply is the model's own turn");
    }

    #[tokio::test]
    async fn snap_flags_fall_on_every_eighth_turn_and_stay_when_a_turn_is_appended() {
        let kernel = Kernel::new_ephemeral("proj-snap").await;
        let ctx = live_context(&kernel, "voice");
        let texts: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let ids = append_dialogue(&kernel, ctx, &refs[..16]);
        let before = project("voice", &kernel.blocks().block_snapshots(ctx).unwrap(), &*kernel.kernel_db().lock()).unwrap();
        let snapped: Vec<usize> =
            before.turns.iter().enumerate().filter(|(_, t)| t.snap).map(|(i, _)| i).collect();
        assert_eq!(snapped, vec![7, 15]);
        append(&kernel, ctx, ids.last(), Role::User, BlockKind::Text, Status::Done, "t16");
        let after = project("voice", &kernel.blocks().block_snapshots(ctx).unwrap(), &*kernel.kernel_db().lock()).unwrap();
        assert_eq!(after.turns.len(), 17);
        assert_eq!(
            before.turns.iter().map(|t| t.snap).collect::<Vec<_>>(),
            after.turns[..16].iter().map(|t| t.snap).collect::<Vec<_>>()
        );
        assert!(!after.turns[16].snap);
    }

    #[tokio::test]
    async fn an_empty_context_projects_the_framing_alone() {
        let kernel = Kernel::new_ephemeral("proj-empty").await;
        let ctx = live_context(&kernel, "system-rules");
        let p = project("system-rules", &kernel.blocks().block_snapshots(ctx).unwrap(), &*kernel.kernel_db().lock()).unwrap();
        assert!(p.turns.is_empty());
        assert!(p.system.contains("\"system-rules\""), "{}", p.system);
    }

    fn projected(kernel: &Kernel, label: &str, ctx: kaijutsu_types::ContextId) -> ContextPut {
        project(label, &kernel.blocks().block_snapshots(ctx).unwrap(), &*kernel.kernel_db().lock()).unwrap()
    }

    #[tokio::test]
    async fn a_voice_reads_drift_as_user_turns_naming_the_sender_and_source() {
        let kernel = Kernel::new_ephemeral("proj-drift").await;
        let banto = character(&kernel, "banto", false);
        let seat = live_context(&kernel, "banto-seat");
        let voice = live_context(&kernel, "council-banto");
        let ids = append_dialogue(&kernel, voice, &["keep the coders on the parser work"]);
        let d1 = append_drift(&kernel, voice, ids.last(), banto, seat, Some("qwen3"), "only touch crates/kaish-parser");
        let unlabeled = kaijutsu_types::ContextId::new();
        let d2 = append_drift(&kernel, voice, Some(&d1), PrincipalId::new(), unlabeled, None, "no pushes today");
        let gone = append_drift(&kernel, voice, Some(&d2), banto, seat, None, "excluded direction");
        kernel.blocks().set_excluded(voice, &gone, true).unwrap();
        append(&kernel, voice, Some(&gone), Role::User, BlockKind::Text, Status::Done, "after the drift");

        let p = projected(&kernel, "council-banto", voice);
        assert_eq!(
            contents(&p),
            [
                "keep the coders on the parser work",
                "From banto (model qwen3), by drift from context \"banto-seat\":\nonly touch crates/kaish-parser",
                &*format!(
                    "From principal {}, by drift from context {}:\nno pushes today",
                    d2.principal_id.short(),
                    unlabeled.short()
                ),
                "after the drift",
            ]
        );
        assert!(p.turns.iter().all(|t| t.role == WireRole::User), "a drift is a user turn");
    }

    fn seat(kernel: &Kernel, ctx: kaijutsu_types::ContextId, budget: u64) -> Option<ContextPut> {
        project_seat(&kernel.blocks().block_snapshots(ctx).unwrap(), budget)
    }

    /// The seat context is the brief, the prompts the seat received since,
    /// and the newest finished narration, each in one turn, oldest first.
    /// Tool calls and results, thinking, system text, and unfinished or
    /// excluded blocks stay out.
    ///
    /// Falsified by a projection that reads the first prompt alone: the later
    /// user note never reaches the body.
    #[tokio::test]
    async fn the_seat_context_is_the_brief_and_the_finished_narration() {
        let kernel = Kernel::new_ephemeral("proj-seat").await;
        let ctx = live_context(&kernel, "lane-a");
        let mut last = Some(append(&kernel, ctx, None, Role::System, BlockKind::Text, Status::Done, "you are a coder"));
        for (role, kind, status, text) in [
            (Role::User, BlockKind::Text, Status::Done, "recover the records from main.db"),
            (Role::Model, BlockKind::Thinking, Status::Done, "pondering"),
            (Role::Model, BlockKind::Text, Status::Done, "The WAL is XORed with 0x42."),
            (Role::Model, BlockKind::ToolCall, Status::Done, "xxd main.db-wal"),
            (Role::Tool, BlockKind::ToolResult, Status::Done, "00000000: 3d 3d"),
            (Role::User, BlockKind::Text, Status::Done, "a later user note"),
            (Role::Model, BlockKind::Text, Status::Done, "excluded narration"),
            (Role::Model, BlockKind::Text, Status::Done, "No backup exists yet."),
            (Role::Model, BlockKind::Text, Status::Running, "mid-stream"),
        ] {
            last = Some(append(&kernel, ctx, last.as_ref(), role, kind, status, text));
        }
        let excluded = kernel
            .blocks()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.content == "excluded narration")
            .unwrap()
            .id;
        kernel.blocks().set_excluded(ctx, &excluded, true).unwrap();

        let p = seat(&kernel, ctx, 2000).expect("a seat with narration has a seat context");
        assert_eq!(
            contents(&p),
            [
                "The seat's brief:\n\nrecover the records from main.db",
                "The prompts the seat has received since, oldest first:\n\na later user note",
                "What the seat has written since, oldest first:\n\nThe WAL is XORed with 0x42.\n\nNo backup exists yet.",
            ]
        );
        assert!(p.turns.iter().all(|t| t.role == WireRole::User), "the seat's words are not the council's own");
        assert_eq!(p.turns.iter().map(|t| t.snap).collect::<Vec<_>>(), [true, false, false], "only the brief keeps a boundary");
        assert!(p.system.contains("proposing seat"), "{}", p.system);
        assert!(p.system.contains("grants no permission"), "{}", p.system);
    }

    /// A seat that narrates nothing still has a seat context once it has a
    /// brief: the council learns what the seat was asked. A seat with no
    /// finished prompt or narration has none.
    ///
    /// Falsified by a projection that waits for narration: a brief and a
    /// tool call give no seat context.
    #[tokio::test]
    async fn a_seat_with_a_brief_and_no_narration_has_a_seat_context() {
        let kernel = Kernel::new_ephemeral("proj-seat-none").await;
        let ctx = live_context(&kernel, "lane-a");
        assert!(seat(&kernel, ctx, 2000).is_none(), "an empty seat");
        let brief = append(&kernel, ctx, None, Role::User, BlockKind::Text, Status::Done, "the brief");
        let call = append(&kernel, ctx, Some(&brief), Role::Model, BlockKind::ToolCall, Status::Done, "ls");
        append(&kernel, ctx, Some(&call), Role::Model, BlockKind::Text, Status::Running, "thinking out loud");
        let p = seat(&kernel, ctx, 2000).expect("a brief makes a seat context");
        assert_eq!(contents(&p), ["The seat's brief:\n\nthe brief"]);
        assert_eq!(p.turns[0].snap, true);
    }

    /// The narration keeps whole blocks, newest first, while they fit the
    /// budget at four bytes a token; a newest block larger than the whole
    /// budget keeps its end. The brief keeps its start.
    #[tokio::test]
    async fn the_seat_narration_keeps_the_newest_whole_blocks_within_the_budget() {
        let kernel = Kernel::new_ephemeral("proj-seat-budget").await;
        let ctx = live_context(&kernel, "lane-a");
        let brief = format!("B{}", "b".repeat(99));
        let mut last = append(&kernel, ctx, None, Role::User, BlockKind::Text, Status::Done, &brief);
        for text in ["a".repeat(30), "c".repeat(30), "d".repeat(30)] {
            last = append(&kernel, ctx, Some(&last), Role::Model, BlockKind::Text, Status::Done, &text);
        }
        // 16 tokens is 64 bytes: the two newest 30-byte blocks fit, the third does not.
        let p = seat(&kernel, ctx, 16).unwrap();
        assert_eq!(p.turns[0].content, format!("The seat's brief:\n\n{}...", &brief[..64]));
        assert_eq!(
            p.turns[1].content,
            format!("What the seat has written since, oldest first:\n\n{}\n\n{}", "c".repeat(30), "d".repeat(30))
        );

        let long = format!("{}END", "e".repeat(200));
        append(&kernel, ctx, Some(&last), Role::Model, BlockKind::Text, Status::Done, &long);
        let p = seat(&kernel, ctx, 16).unwrap();
        let narration = p.turns[1].content.strip_prefix("What the seat has written since, oldest first:\n\n").unwrap();
        assert!(narration.starts_with("...") && narration.ends_with("END"), "{narration}");
        assert_eq!(narration.len(), 3 + 64);
    }

    #[tokio::test]
    async fn drift_stays_out_of_the_system_rules_and_other_council_contexts() {
        let kernel = Kernel::new_ephemeral("proj-nodrift").await;
        let banto = character(&kernel, "banto", false);
        let seat = live_context(&kernel, "banto-seat");
        for label in ["council-system", "voice", "system-rules"] {
            let ctx = live_context(&kernel, label);
            let ids = append_dialogue(&kernel, ctx, &["a house rule"]);
            append_drift(&kernel, ctx, ids.last(), banto, seat, None, "a drifted direction");
            assert_eq!(contents(&projected(&kernel, label, ctx)), ["a house rule"], "{label}");
        }
        assert!(is_voice("council-amy") && !is_voice("council-system") && !is_voice("voice"));
    }
}
