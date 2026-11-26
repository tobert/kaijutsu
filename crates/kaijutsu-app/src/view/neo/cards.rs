use bevy::prelude::*;
use kaijutsu_client::ContextMirror;
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, Role, Status};

use crate::text::diff::{DiffLineClass, DiffPreview};
use crate::ui::msdf_panel::{truncate, wrap};
use crate::ui::quick_context::{LineTone, PanelLine};
use crate::ui::screen::Screen;
use crate::view::{ConversationScrollState, DocumentCache, FocusTarget, MainCell};
use super::diff_preview::{Previews, preview};
use crate::view::geometry::ConversationGeometry;

use super::{NeoMode, Workspace, diff_preview};
use super::model::{Record, RecordState, Section};
use super::score_preview::{ScorePreview, ScorePreviewCache};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ThreadLink {
    pub call: BlockId,
    pub receipt: Option<BlockId>,
    pub patch: Option<BlockId>,
}

#[derive(Resource, Default, PartialEq, Eq)]
pub(super) struct SceneContent {
    pub conversation: Vec<PanelLine>,
    pub artifacts: Vec<Vec<PanelLine>>,
    pub score: Option<ScorePreview>,
    pub events: Vec<Vec<PanelLine>>,
    pub thread: Option<ThreadLink>,
    pub notch_line: Option<usize>,
}

pub(super) fn sync(
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    workspace: Res<Workspace>,
    documents: Res<DocumentCache>,
    focus: Res<FocusTarget>,
    scroll: Res<ConversationScrollState>,
    geometry: Query<&ConversationGeometry, With<MainCell>>,
    mut cards: ResMut<SceneContent>,
    mut seen: Local<Option<(ContextId, u64, Option<BlockId>)>>,
    mut previews: Local<Previews>,
    mut score: Local<ScorePreviewCache>,
    mut images: ResMut<Assets<Image>>,
    fonts: Option<Res<crate::text::SvgFontDb>>,
) {
    if !mode.enabled || *screen.get() != Screen::Conversation {
        *seen = None;
        return;
    }
    let Some(entry) = workspace.context.and_then(|id| documents.get(id)) else {
        *seen = None;
        previews.clear();
        cards.set_if_neq(SceneContent::default());
        return;
    };
    let anchor = focus.reader_block(&scroll, geometry.single().ok());
    let key = (entry.mirror.context_id(), entry.mirror.version(), anchor);
    if *seen == Some(key) && !workspace.is_changed() {
        return;
    }
    *seen = Some(key);
    let mut content = build(&entry.mirror, &entry.context_name, &workspace, anchor, &mut previews);
    content.score = workspace.scene_artifact().and_then(|id| entry.mirror.block(&id))
        .filter(|block| block.content_type == ContentType::Abc)
        .and_then(|block| score.preview(block, fonts.as_deref(), &mut images));
    cards.set_if_neq(content);
}

fn line(text: impl Into<String>, tone: LineTone) -> PanelLine {
    PanelLine { text: text.into(), tone }
}

fn stack<'a>(rows: impl Iterator<Item = &'a Record>, selected: Option<BlockId>) -> Vec<&'a Record> {
    let rows: Vec<_> = rows.collect();
    let start = rows.iter().position(|row| Some(row.id) == selected).unwrap_or(0);
    rows[start..].iter().chain(rows[..start].iter()).take(3).copied().collect()
}

fn call_receipt<'a>(workspace: &'a Workspace, call: BlockId, patch: Option<BlockId>) -> Option<&'a Record> {
    if let Some(row) = patch.and_then(|id| workspace.artifact_receipt(id))
        .filter(|row| row.kind == BlockKind::ToolResult && row.call_id == Some(call))
    {
        return Some(row);
    }
    workspace.records.history.iter().find(|row| row.kind == BlockKind::ToolResult && row.call_id == Some(call))
}

fn selected_thread(workspace: &Workspace, event: &Record) -> Option<ThreadLink> {
    let patch = workspace.event_diff(event);
    if workspace.section == Section::Workbench && (patch.is_none() || patch != workspace.scene_artifact()) {
        return None;
    }
    match event.kind {
        BlockKind::ToolCall => {
            let receipt = call_receipt(workspace, event.id, patch);
            Some(ThreadLink { call: event.id, receipt: receipt.map(|row| row.id), patch })
        }
        BlockKind::ToolResult => Some(ThreadLink { call: event.call_id?, receipt: Some(event.id), patch }),
        _ => None,
    }
}

pub(super) fn event_detail(
    mirror: &ContextMirror,
    workspace: &Workspace,
    row: &Record,
    patch: Option<&DiffPreview>,
    columns: usize,
) -> Vec<PanelLine> {
    if columns == 0 {
        return Vec::new();
    }
    let diff_id = workspace.event_diff(row);
    let outcome = if row.kind == BlockKind::ToolCall {
        call_receipt(workspace, row.id, diff_id).unwrap_or(row)
    } else { row };
    let tone = match outcome.state {
        RecordState::Waiting => LineTone::Warn,
        RecordState::Failed => LineTone::Bad,
        RecordState::Normal => LineTone::Row,
    };
    let mut lines = Vec::new();
    if let Some(patch) = patch
        && !patch.is_error
    {
        lines.push(line(truncate(&format!("Patch · {}", patch.stat), columns), tone));
        if outcome.state != RecordState::Normal { lines.push(line(truncate(&outcome.summary, columns), tone)); }
        if let Some(file) = diff_preview::first_file(patch) {
            lines.push(line(truncate(file, columns), LineTone::Row));
        }
    } else if outcome.kind == BlockKind::ToolResult && outcome.state != RecordState::Waiting
        && let Some(block) = mirror.block(&outcome.id)
    {
        let status = match block.exit_code {
            Some(code) => format!("exit {code}"),
            None if block.is_error => "failed".into(),
            None => block.status.as_str().to_owned(),
        };
        lines.push(line(truncate(&status, columns), tone));
        let stderr = block.stderr.as_deref().unwrap_or_default();
        let output = stderr.lines().chain(block.content.lines()).map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().any(|c| !"{}[],".contains(c)));
        lines.extend(output.take(3).map(|s| line(truncate(s, columns), LineTone::Row)));
    } else {
        lines.extend(wrap(&outcome.summary, columns, 0).into_iter().take(3).map(|text| line(text, tone)));
    }
    lines
}

fn build(mirror: &ContextMirror, name: &str, workspace: &Workspace, anchor: Option<BlockId>, cache: &mut Previews) -> SceneContent {
    cache.retain(|id, _| workspace.diffs().any(|row| row.id == *id));
    let event = workspace.scene_event();
    let thread = event.and_then(|row| selected_thread(workspace, row)).filter(|link| {
        mirror.block(&link.call).is_some_and(|block| block.kind == BlockKind::ToolCall && !block.excluded && block.status != Status::Draft)
    });
    let (conversation, notch_line) = dialogue(mirror, name, workspace, anchor, thread);
    let mut cards = SceneContent {
        conversation, notch_line, thread,
        ..default()
    };
    for row in stack(workspace.scene_artifacts(), workspace.scene_artifact()) {
        if row.content_type == ContentType::Abc {
            cards.artifacts.push(vec![
                line(truncate(&row.title, 52), LineTone::Head),
                line("No notation to preview", LineTone::Row),
            ]);
            continue;
        }
        let Some(patch) = preview(mirror, row.id, cache) else { continue };
        let mut body = diff_preview::rows(patch, 49, 9);
        let header = body.iter().position(|line| line.tone == LineTone::Diff(DiffLineClass::FileHeader))
            .map(|index| body.remove(index).text).unwrap_or_else(|| row.title.clone());
        body.insert(0, line(header, LineTone::Head));
        cards.artifacts.push(body);
    }
    for row in stack(workspace.records.history.iter(), event.map(|row| row.id)) {
        let patch = workspace.event_diff(row).and_then(|id| preview(mirror, id, cache));
        let mut lines = vec![line(truncate(&row.title, 52), LineTone::Head)];
        lines.extend(event_detail(mirror, workspace, row, patch, 49));
        cards.events.push(lines);
    }
    cards
}

/// Number calls from their actual log positions. A notch between passages
/// records when a call was made; the passage text does not establish authorship.
fn dialogue(mirror: &ContextMirror, name: &str, workspace: &Workspace, anchor: Option<BlockId>, link: Option<ThreadLink>) -> (Vec<PanelLine>, Option<usize>) {
    let blocks = mirror.blocks();
    let end = anchor.and_then(|id| blocks.iter().rposition(|block| block.id == id))
        .map_or(blocks.len(), |index| index + 1);
    let is_dialogue = |block: &BlockSnapshot| {
        matches!(block.role, Role::User | Role::Model) && block.kind == BlockKind::Text
            && !block.excluded && block.status != Status::Draft
            && matches!(block.content_type, ContentType::Plain | ContentType::Markdown)
            && !block.content.trim().is_empty()
            && !workspace.records.workbench.iter().any(|artifact| artifact.id == block.id)
    };
    let mut passages: Vec<_> = (0..end).rev().filter(|&i| is_dialogue(&blocks[i])).take(3).collect();
    if passages.is_empty() {
        passages.extend((end..blocks.len()).filter(|&i| is_dialogue(&blocks[i])).take(3));
    } else {
        passages.reverse();
    }
    let call_index = link.and_then(|link| blocks.iter().rposition(|block| block.id == link.call));
    let start = passages.first().copied().unwrap_or(end);
    let stop = passages.last().map_or(end, |i| (i + 1).max(end));
    let mut gaps = vec![None; passages.len() + 1];
    let mut from = start;
    for (i, until) in passages.iter().copied().chain(std::iter::once(stop)).enumerate() {
        let count = blocks[from..until].iter().filter(|b| b.kind == BlockKind::ToolCall && !b.excluded && b.status != Status::Draft).count();
        if count > 0 {
            let noun = if count == 1 { "call" } else { "calls" };
            gaps[i] = Some((format!("· {count} {noun}"), false));
        }
        from = until + 1;
    }
    if let Some(index) = call_index {
        let slot = passages.partition_point(|&i| i < index);
        let prefix = if index < start { "Earlier call" } else if index >= stop { "Later call" } else { "Call" };
        let block = &blocks[index];
        let subject = super::model::command_arg(block).or_else(|| block.tool_name.clone()).unwrap_or_default();
        gaps[slot] = Some((truncate(&format!("{prefix} · {subject}"), 42), true));
    }

    let title = if name.is_empty() { "Conversation".into() } else { format!("Conversation · {name}") };
    let mut lines = vec![line(title, LineTone::Head)];
    let mut notch = None;
    let mut remaining = 18 - 1 - passages.len() - gaps.iter().flatten().count();
    for (i, gap) in gaps.into_iter().enumerate() {
        if let Some((label, selected)) = gap {
            if selected { notch = Some(lines.len()); }
            lines.push(line(label, if selected { LineTone::Head } else { LineTone::Dim }));
        }
        let Some(&index) = passages.get(i) else { break };
        let block = &blocks[index];
        lines.push(line(if block.role == Role::User { "User" } else { "Assistant" }, LineTone::Head));
        let budget = if i + 1 == passages.len() { remaining } else { remaining.min(3) };
        let mut body = block.content.trim().lines().flat_map(|paragraph| {
            let excerpt: String = paragraph.chars().take(1200).collect();
            wrap(&excerpt, 42, 0)
        });
        let mut excerpt: Vec<_> = body.by_ref().take(budget).collect();
        if body.next().is_some() && let Some(last) = excerpt.last_mut() { last.push('…'); }
        remaining -= excerpt.len();
        lines.extend(excerpt.into_iter().map(|text| line(text, LineTone::Row)));
    }
    if passages.is_empty() && notch.is_none() {
        lines.push(line("No dialogue at this position", LineTone::Dim));
    }
    (lines, notch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, ContentType, ContextId, PrincipalId, Role};
    use crate::view::neo::model::{Records, Workspace};
    use crate::view::neo::projection::DetectedDiffs;

    #[test]
    fn selected_patch_follows_its_receipt_to_the_call_position_across_another_turn() {
        let context = ContextId::from_bytes([7; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([3; 16]), seq);
        let patch = "--- a/harbor\n+++ b/harbor\n@@ -1 +1 @@\n-dim\n+bright";
        let trace = vec![
            BlockSnapshotBuilder::new(id(1), BlockKind::Text).role(Role::User).content("Look at the harbor.").build(),
            BlockSnapshotBuilder::new(id(2), BlockKind::ToolCall).tool_name("diff_block").build(),
            BlockSnapshotBuilder::new(id(3), BlockKind::Text).role(Role::User).content("Also check the orchard.").build(),
            BlockSnapshotBuilder::new(id(4), BlockKind::ToolCall).tool_name("inventory").build(),
            BlockSnapshotBuilder::new(id(5), BlockKind::Text).content_type(ContentType::Diff).content(patch).build(),
            BlockSnapshotBuilder::new(id(6), BlockKind::ToolResult).tool_call_id(id(2)).status(Status::Done)
                .content(format!(r#"{{"block_id":"{}"}}"#, id(5).to_key())).build(),
            BlockSnapshotBuilder::new(id(7), BlockKind::ToolResult).tool_call_id(id(4)).content("Later receipt").build(),
            BlockSnapshotBuilder::new(id(8), BlockKind::Text).content_type(ContentType::Diff).content(patch).build(),
        ];
        let mut mirror = kaijutsu_client::ContextMirror::new(context);
        mirror.apply_snapshot(trace, 1).unwrap();
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &Default::default(), &mut DetectedDiffs::default())));
        let standalone = build(&mirror, "Harbor", &workspace, None, &mut Default::default());
        assert_eq!(standalone.thread, None, "identical patch text cannot prove a link");
        workspace.step_scene(true);
        let linked = build(&mirror, "Harbor", &workspace, None, &mut Default::default());
        assert_eq!(linked.thread, Some(ThreadLink { call: id(2), receipt: Some(id(6)), patch: Some(id(5)) }));
        assert!(linked.events[0][0].text.contains("diff_block"), "put the linked receipt in front");
        let notch = linked.notch_line.unwrap();
        assert!(linked.conversation[..notch].iter().any(|line| line.text.contains("Look at the harbor.")));
        assert!(linked.conversation[notch + 1..].iter().any(|line| line.text.contains("Also check the orchard.")));
        workspace.switch_scene_section();
        workspace.selected = Some(id(7));
        let inventory = build(&mirror, "Harbor", &workspace, None, &mut Default::default());
        assert_eq!(inventory.thread, Some(ThreadLink { call: id(4), receipt: Some(id(7)), patch: None }),
            "an unrelated event keeps the patch on screen without claiming it");
    }

    #[test]
    fn call_with_a_patch_uses_the_status_of_the_receipt_that_returned_it() {
        let context = ContextId::from_bytes([4; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([3; 16]), seq);
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![
            BlockSnapshotBuilder::new(id(1), BlockKind::ToolCall).tool_name("diff_block").build(),
            BlockSnapshotBuilder::new(id(2), BlockKind::Text).content_type(ContentType::Diff)
                .content("--- a/a\n+++ b/a\n@@ -1 +1 @@\n-one\n+two").build(),
            BlockSnapshotBuilder::new(id(3), BlockKind::ToolResult).tool_call_id(id(1)).status(Status::Done)
                .content(format!(r#"{{"block_id":"{}"}}"#, id(2).to_key())).build(),
            BlockSnapshotBuilder::new(id(4), BlockKind::ToolResult).tool_call_id(id(1)).is_error(true)
                .status(Status::Error).content("separate failure").build(),
        ], 1).unwrap();
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &Default::default(), &mut DetectedDiffs::default())));
        workspace.switch_section();
        workspace.selected = Some(id(1));
        let cards = build(&mirror, "", &workspace, None, &mut Default::default());
        assert_eq!(cards.thread.unwrap().receipt, Some(id(3)));
        assert!(cards.events[0].iter().any(|line| line.text.contains("Patch") && line.tone == LineTone::Row));
    }

    #[test]
    fn event_detail_fits_the_available_columns_and_keeps_wait_tone() {
        let context = ContextId::from_bytes([5; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([2; 16]), seq);
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![
            BlockSnapshotBuilder::new(id(1), BlockKind::ToolCall).tool_name("slow_shell")
                .status(Status::Waiting).build(),
            BlockSnapshotBuilder::new(id(2), BlockKind::ToolCall).tool_name("shell").build(),
            BlockSnapshotBuilder::new(id(3), BlockKind::ToolResult).tool_call_id(id(2))
                .exit_code(3).is_error(true).content("the output includes a very long filename").build(),
            BlockSnapshotBuilder::new(id(4), BlockKind::ToolCall).tool_name("diff_block").build(),
            BlockSnapshotBuilder::new(id(5), BlockKind::ToolResult).tool_call_id(id(4))
                .status(Status::Done).content_type(ContentType::Diff)
                .content("--- a/a-very-long-filename\n+++ b/a-very-long-filename\n@@ -1 +1 @@\n-a\n+b").build(),
        ], 1).unwrap();
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &Default::default(), &mut DetectedDiffs::default())));
        let row = |id| workspace.records.history.iter().find(|row| row.id == id).unwrap();
        let waiting = event_detail(&mirror, &workspace, row(id(1)), None, 8);
        assert!(!waiting.is_empty() && waiting.iter().all(|line| line.tone == LineTone::Warn));
        let failed = event_detail(&mirror, &workspace, row(id(2)), None, 8);
        assert_eq!(failed[0], line("exit 3", LineTone::Bad));
        assert_eq!(failed.len(), 2);
        assert!(waiting.iter().chain(&failed).all(|line| line.text.chars().count() <= 8));
        let mut previews = Previews::default();
        let patch = preview(&mirror, workspace.event_diff(row(id(4))).unwrap(), &mut previews);
        let patch_rows = event_detail(&mirror, &workspace, row(id(4)), patch, 8);
        assert!(patch_rows[0].text.starts_with("Patch"));
        assert!(patch_rows.iter().all(|line| line.text.chars().count() <= 8));
    }

    #[test]
    fn a_call_card_shows_its_receipt_output_and_exit_status() {
        let context = ContextId::from_bytes([8; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([2; 16]), seq);
        let call = BlockSnapshotBuilder::new(id(1), BlockKind::ToolCall).tool_name("shell")
            .tool_input(r#"{"command":"check"}"#).build();
        let result = BlockSnapshotBuilder::new(id(2), BlockKind::ToolResult).tool_call_id(id(1))
            .exit_code(3).is_error(true).content("{\n  \"missing\": \"orchard.txt\"\n}").build();
        let mut mirror = kaijutsu_client::ContextMirror::new(context);
        mirror.apply_snapshot(vec![call, result], 1).unwrap();
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &Default::default(), &mut DetectedDiffs::default())));
        let cards = build(&mirror, "Review", &workspace, None, &mut Default::default());
        let call_card = &cards.events[1];
        assert!(call_card.iter().any(|line| line.text.contains("exit 3") && line.tone == LineTone::Bad));
        assert!(call_card.iter().any(|line| line.text.contains("orchard.txt")));
    }

    #[test]
    fn cards_show_dialogue_near_the_reader_patch_content_and_event_outcome() {
        let context = ContextId::from_bytes([3; 16]);
        let id = |seq| BlockId::new(context, PrincipalId::from_bytes([1; 16]), seq);
        let patch = "--- a/sample.rs\n+++ b/sample.rs\n@@ -1 +1 @@\n-old\n+actual change";
        let blocks = vec![
            BlockSnapshotBuilder::new(id(0), BlockKind::Trace).content("Started").build(),
            BlockSnapshotBuilder::new(id(1), BlockKind::Text).role(Role::User).content("Check the diff.").build(),
            BlockSnapshotBuilder::new(id(2), BlockKind::Text).role(Role::Model).content("Here is the first change.").build(),
            BlockSnapshotBuilder::new(id(3), BlockKind::ToolCall).tool_name("shell").tool_input(r#"{"command":"diff -u a b"}"#).build(),
            BlockSnapshotBuilder::new(id(4), BlockKind::ToolResult).tool_call_id(id(3)).content_type(ContentType::Diff).content(patch).build(),
            BlockSnapshotBuilder::new(id(5), BlockKind::Text).role(Role::User).content("Much later.").build(),
        ];
        let mut mirror = kaijutsu_client::ContextMirror::new(context);
        mirror.apply_snapshot(blocks, 1).unwrap();
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &Default::default(), &mut DetectedDiffs::default())));
        let cards = build(&mirror, "Review", &workspace, Some(id(2)), &mut Default::default());
        let text = |lines: &[PanelLine]| lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        let dialogue = text(&cards.conversation);
        assert!(dialogue.contains("Check the diff.") && dialogue.contains("Here is the first change."));
        assert!(!dialogue.contains("Much later."));
        let beginning = build(&mirror, "Review", &workspace, Some(id(0)), &mut Default::default());
        assert!(text(&beginning.conversation).contains("Check the diff."), "opening at a system prelude shows the first exchange");
        assert!(cards.artifacts[0][0].text.contains("sample.rs"));
        assert!(text(&cards.artifacts[0]).contains("+actual change"));
        assert!(text(&cards.events[0]).contains("diff -u a b"));
        assert!(text(&cards.events[0]).contains("1 file, +1"));
    }
}
