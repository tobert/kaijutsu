//! Projects a kaijutsu context into the council server's context body.
//!
//! The body is the context's guidance as the server reads it: a fixed framing
//! as the system message, then the context's conversational text in document
//! order. A block reaches the body when it is finished text a player or model
//! wrote (`BlockKind::Text`, `Status::Done`, `Role::User` or `Role::Model`)
//! and nobody excluded it (`kj stage exclude`) or marked it ephemeral. System
//! instructions, thinking, tool calls and results, drift, and unsubmitted
//! drafts stay out.

use kaijutsu_council::wire::{ContextPut, Role as WireRole, Turn};
use kaijutsu_types::{BlockKind, BlockSnapshot, Role, Status};

/// A turn carries `snap` when its 1-based position is a multiple of this.
///
/// The server extends a context only from a snapshot boundary, and each
/// boundary costs recurrent state on the server. Deriving the flag from the
/// turn's position alone keeps every earlier boundary where it was when turns
/// are appended, so a change at the tail re-feeds at most this many turns.
pub(crate) const SNAP_EVERY: usize = 8;

/// The framing every council context carries as its system message; `{label}`
/// is the context's label.
const FRAMING: &str = "You review shell commands that coding agents propose in kaijutsu. \
This conversation is the council context \"{label}\": guidance to judge them by.";

/// The server's body for the context labeled `label`, projected from its
/// `blocks` in document order.
pub(crate) fn project(label: &str, blocks: &[BlockSnapshot]) -> ContextPut {
    let turns = blocks
        .iter()
        .filter(|b| reaches_council(b))
        .enumerate()
        .map(|(i, b)| Turn {
            role: match b.role {
                Role::Model => WireRole::Assistant,
                _ => WireRole::User,
            },
            content: b.content.clone(),
            snap: (i + 1) % SNAP_EVERY == 0,
        })
        .collect();
    ContextPut {
        system: FRAMING.replace("{label}", label),
        turns,
        pin: None,
        warm: None,
        dry_run: None,
    }
}

fn reaches_council(b: &BlockSnapshot) -> bool {
    b.kind == BlockKind::Text
        && b.status == Status::Done
        && matches!(b.role, Role::User | Role::Model)
        && !b.excluded
        && !b.ephemeral
        && !b.content.is_empty()
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
        let id = ContextId::new();
        let row = ContextRow {
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
    use kaijutsu_types::{BlockKind, Role, Status};

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
        let p = project("voice", &blocks);
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
            (Role::Model, BlockKind::Thinking, Status::Done, "pondering"),
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
        let p = project("voice", &blocks);
        assert_eq!(contents(&p), ["keep one", "keep two", "keep three"]);
    }

    #[tokio::test]
    async fn snap_flags_fall_on_every_eighth_turn_and_stay_when_a_turn_is_appended() {
        let kernel = Kernel::new_ephemeral("proj-snap").await;
        let ctx = live_context(&kernel, "voice");
        let texts: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let ids = append_dialogue(&kernel, ctx, &refs[..16]);
        let before = project("voice", &kernel.blocks().block_snapshots(ctx).unwrap());
        let snapped: Vec<usize> =
            before.turns.iter().enumerate().filter(|(_, t)| t.snap).map(|(i, _)| i).collect();
        assert_eq!(snapped, vec![7, 15]);
        append(&kernel, ctx, ids.last(), Role::User, BlockKind::Text, Status::Done, "t16");
        let after = project("voice", &kernel.blocks().block_snapshots(ctx).unwrap());
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
        let p = project("system-rules", &kernel.blocks().block_snapshots(ctx).unwrap());
        assert!(p.turns.is_empty());
        assert!(p.system.contains("\"system-rules\""), "{}", p.system);
    }
}
