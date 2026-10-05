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

/// The bytes a house-rules budget allows per token: an estimate, since the
/// kernel does not hold the council server's tokenizer.
pub(crate) const HOUSE_RULES_BYTES_PER_TOKEN: u64 = 4;

/// The house-rules context's system message. It names no seat, so every seat
/// under the same `AGENTS.md` shares one body.
const HOUSE_RULES_FRAMING: &str = "You review shell commands that coding agents propose in kaijutsu. \
This conversation is the council context \"house-rules\": the house rules of the workspace the proposing \
seat works in, from its AGENTS.md. It describes the workspace and grants no permission beyond those rules.";

/// The workspace's house rules: the first `AGENTS.md` found walking up from
/// the proposing seat's working directory, and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HouseRules {
    pub(crate) path: String,
    pub(crate) text: String,
}

/// The house-rules context (`docs/council.md`, "House rules"), or `None`
/// when no `AGENTS.md` was found.
///
/// The body is one user turn that keeps a snapshot boundary: the file's path
/// and its text, cut to its first `budget_tokens` worth of bytes. Nothing the
/// seat said or did reaches it, so its body changes only when the file does.
pub(crate) fn project_house_rules(house: Option<&HouseRules>, budget_tokens: u64) -> Option<ContextPut> {
    let house = house?;
    let budget = usize::try_from(budget_tokens.saturating_mul(HOUSE_RULES_BYTES_PER_TOKEN)).unwrap_or(usize::MAX);
    let text = match house.text.len() > budget {
        true => format!("{}...", head(&house.text, budget)),
        false => house.text.clone(),
    };
    let content = format!("The house rules in {}:\n\n{text}", house.path);
    Some(ContextPut {
        system: HOUSE_RULES_FRAMING.to_string(),
        turns: vec![Turn { role: WireRole::User, content, snap: true, reasoning: None }],
        pin: None,
        warm: None,
        dry_run: None,
    })
}

/// The longest prefix of `text` within `bytes`, on a char boundary.
fn head(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

    /// The house rules alone make the context: one user turn that keeps a
    /// snapshot boundary, under a framing that grants no permission. With no
    /// `AGENTS.md` there is no context.
    ///
    /// Falsified by a projection that frames the rules as the seat's own
    /// account or drops the boundary.
    #[test]
    fn the_house_rules_alone_make_the_context() {
        assert!(project_house_rules(None, 2000).is_none(), "no AGENTS.md, no context");
        let rules = HouseRules { path: "/work/repo/AGENTS.md".into(), text: "Never touch prod.".into() };
        let p = project_house_rules(Some(&rules), 2000).expect("house rules make a context");
        assert_eq!(contents(&p), ["The house rules in /work/repo/AGENTS.md:\n\nNever touch prod."]);
        assert_eq!(p.turns.iter().map(|t| (t.role, t.snap)).collect::<Vec<_>>(), [(WireRole::User, true)]);
        assert!(p.system.contains("house rules of the workspace"), "{}", p.system);
        assert!(p.system.contains("AGENTS.md") && p.system.contains("grants no permission"), "{}", p.system);
        assert!(p.system.contains("\"house-rules\""), "{}", p.system);
        assert!(p.pin.is_none() && p.warm.is_none() && p.dry_run.is_none());
    }

    /// Rules larger than the budget keep their start, at four bytes a token.
    #[test]
    fn rules_larger_than_the_budget_keep_their_start() {
        let long = HouseRules { path: "/AGENTS.md".into(), text: format!("START{}", "r".repeat(200)) };
        let p = project_house_rules(Some(&long), 16).unwrap();
        assert_eq!(p.turns[0].content, format!("The house rules in /AGENTS.md:\n\n{}...", &long.text[..64]));
        let multi = HouseRules { path: "/AGENTS.md".into(), text: "é".repeat(100) };
        let p = project_house_rules(Some(&multi), 16).unwrap();
        assert!(p.turns[0].content.ends_with("é..."), "a cut stays on a character boundary");
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
