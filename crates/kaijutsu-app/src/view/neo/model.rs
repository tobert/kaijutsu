use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_client::ContextMirror;
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, Status};

use crate::view::surface::chrome::pending_tool_ask;

use super::projection::{DetectedDiffs, NeoProjection};

const LOOKBACK: usize = 1_024;
const MAX_RECORDS: usize = 48;
const MAX_SUMMARY: usize = 96;

#[derive(Component, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Section {
    #[default]
    Workbench,
    History,
}

impl Section {
    pub(super) fn other(self) -> Self {
        match self {
            Self::Workbench => Self::History,
            Self::History => Self::Workbench,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub id: BlockId,
    pub kind: BlockKind,
    pub call_id: Option<BlockId>,
    pub artifact_id: Option<BlockId>,
    pub excluded: bool,
    pub content_type: ContentType,
    pub title: String,
    pub summary: String,
    pub status: Status,
    pub state: RecordState,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RecordState {
    Normal,
    Waiting,
    Failed,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Records {
    pub workbench: Vec<Record>,
    pub history: Vec<Record>,
}

impl Records {
    pub(super) fn from_mirror(mirror: &ContextMirror, waiting_on: &HashMap<BlockId, String>, cache: &mut DetectedDiffs) -> Self {
        let blocks = mirror.blocks();
        let projection = NeoProjection::from_blocks(&blocks[blocks.len().saturating_sub(LOOKBACK)..], cache);
        let collect = |events: Vec<super::projection::NeoEvent>, section| {
            events.into_iter().rev().take(MAX_RECORDS).filter_map(|event| {
                let block = mirror.block(&event.id)?;
                let call = if block.kind == BlockKind::ToolCall {
                    Some(block)
                } else {
                    block.tool_call_id.and_then(|id| mirror.block(&id))
                        .filter(|call| call.kind == BlockKind::ToolCall)
                };
                Some(record(block, call, section, event.content_type, pending_tool_ask(block, waiting_on)))
            }).collect()
        };
        let workbench: Vec<Record> = collect(projection.artifacts, Section::Workbench);
        let mut history: Vec<Record> = collect(projection.history, Section::History);
        let mut links = HashMap::new();
        for row in &history {
            let Some(call) = row.call_id.and_then(|id| mirror.block(&id)) else { continue };
            let Some(result) = mirror.block(&row.id) else { continue };
            if let Some(id) = created_artifact(result, call, &workbench) {
                links.insert(result.id, id);
                links.entry(call.id).or_insert(id);
            }
        }
        for row in &mut history {
            row.artifact_id = links.get(&row.id).copied();
        }
        Self { workbench, history }
    }

    pub(super) fn in_section(&self, section: Section) -> &[Record] {
        match section {
            Section::Workbench => &self.workbench,
            Section::History => &self.history,
        }
    }
}

fn creator_type(name: &str) -> Option<ContentType> {
    Some(match name {
        "diff_block" => ContentType::Diff,
        "svg_block" => ContentType::Svg,
        "abc_block" => ContentType::Abc,
        "img_block" | "img_block_from_path" => ContentType::Media,
        _ => return None,
    })
}

fn created_artifact(result: &BlockSnapshot, call: &BlockSnapshot, workbench: &[Record]) -> Option<BlockId> {
    if result.status != Status::Done || result.is_error || result.exit_code.is_some_and(|code| code != 0) {
        return None;
    }
    let tool = call.tool_name.as_deref()?;
    let shell = is_shell(Some(tool));
    if !shell && creator_type(tool).is_none() { return None; }
    let receipt: serde_json::Value = serde_json::from_str(&result.content).ok()?;
    let id = BlockId::from_key(receipt.get("block_id")?.as_str()?)?;
    let artifact = workbench.iter().find(|row| row.id == id)?;
    let expected = if shell {
        let input: serde_json::Value = serde_json::from_str(call.tool_input.as_deref().unwrap_or(&call.content)).ok()?;
        let source = input.get("code").or_else(|| input.get("command"))?.as_str()?;
        let program = kaish_kernel::parser::parse(source).ok()?;
        let [kaish_kernel::ast::Stmt::Command(command)] = program.statements.as_slice() else { return None };
        if !command.redirects.is_empty() { return None; }
        creator_type(&command.name)?
    } else {
        creator_type(tool)?
    };
    (artifact.content_type == expected).then_some(id)
}

#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub(super) struct Workspace {
    pub context: Option<ContextId>,
    pub ready: bool,
    pub section: Section,
    pub selected: Option<BlockId>,
    other_selection: Option<BlockId>,
    pub records: Records,
}

impl Workspace {
    pub fn reconcile(&mut self, context: Option<ContextId>, records: Option<Records>) {
        if self.context != context {
            self.context = context;
            self.section = Section::default();
            self.selected = None;
            self.other_selection = None;
        }
        self.ready = records.is_some();
        self.records = records.unwrap_or_default();
        self.selected = self.retained(self.section, self.selected);
        self.other_selection = self.retained(self.section.other(), self.other_selection);
    }

    pub fn selection(&self, section: Section) -> Option<BlockId> {
        if section == self.section { self.selected } else { self.other_selection }
    }

    pub fn workbench_source(&self) -> Option<BlockId> {
        if self.section == Section::History && let Some(event) = self.history_source()
            && let Some(artifact) = self.event_artifact(event, self.records.workbench.iter())
        {
            return Some(artifact.id);
        }
        self.selection(Section::Workbench).or_else(|| self.records.workbench.first().map(|row| row.id))
    }

    pub fn artifact_receipt(&self, id: BlockId) -> Option<&Record> {
        let artifact = self.records.workbench.iter().find(|row| row.id == id)?;
        let mut matching = self.records.history.iter().filter(|row| Self::event_has_artifact(row, artifact));
        let command_diff = artifact.kind == BlockKind::Trace && artifact.call_id.is_some();
        matching.clone().find(|row| row.kind == BlockKind::ToolResult && (!command_diff || row.status.is_terminal()))
            .or_else(|| matching.find(|row| row.kind == BlockKind::ToolCall))
    }

    pub fn history_source(&self) -> Option<&Record> {
        if self.section == Section::Workbench {
            let id = self.selection(Section::Workbench).or_else(|| self.records.workbench.first().map(|row| row.id))?;
            return self.artifact_receipt(id);
        }
        self.records.history.iter().find(|row| Some(row.id) == self.selected).or_else(|| self.records.history.first())
    }

    pub fn switch_section(&mut self) {
        let artifact = self.workbench_source();
        let event = self.history_source().map(|row| row.id).or(self.selection(Section::History));
        self.section = self.section.other();
        (self.selected, self.other_selection) = match self.section {
            Section::History => (event, artifact),
            Section::Workbench => (artifact, event),
        };
    }

    pub fn step(&mut self, forward: bool) {
        if self.section == Section::History {
            self.other_selection = self.workbench_source();
        }
        self.step_record(forward);
    }

    fn step_record(&mut self, forward: bool) {
        let records = self.records.in_section(self.section);
        let current = records.iter().position(|row| Some(row.id) == self.selected).unwrap_or(0);
        let next = if forward { (current + 1).min(records.len().saturating_sub(1)) } else { current.saturating_sub(1) };
        self.selected = records.get(next).map(|row| row.id);
    }

    pub fn diffs(&self) -> impl DoubleEndedIterator<Item = &Record> {
        self.records.workbench.iter().filter(|row| row.content_type == ContentType::Diff)
    }

    fn event_has_artifact(event: &Record, artifact: &Record) -> bool {
        Some(artifact.id) == event.artifact_id || artifact.id == event.id
            || artifact.call_id == Some(event.id)
            || (artifact.kind == BlockKind::Trace && artifact.call_id.is_some() && artifact.call_id == event.call_id)
    }

    fn event_artifact<'a>(
        &self, event: &Record, records: impl Iterator<Item = &'a Record>,
    ) -> Option<&'a Record> {
        let selected = self.selection(Section::Workbench);
        let mut matches = records.filter(|row| Self::event_has_artifact(event, row));
        let mut first = None;
        for row in &mut matches {
            if Some(row.id) == selected {
                return Some(row);
            }
            first.get_or_insert(row);
        }
        first
    }

    pub fn event_diff(&self, event: &Record) -> Option<BlockId> {
        self.event_artifact(event, self.diffs()).map(|row| row.id)
    }

    pub fn scene_artifacts(&self) -> impl DoubleEndedIterator<Item = &Record> {
        self.records.workbench.iter().filter(|row| matches!(row.content_type, ContentType::Diff | ContentType::Abc))
    }

    pub fn scene_event(&self) -> Option<&Record> {
        if self.section == Section::Workbench && let Some(artifact) = self.scene_artifact()
            && let Some(receipt) = self.artifact_receipt(artifact).filter(|row| row.call_id.is_some() || row.kind == BlockKind::ToolCall)
        {
            return Some(receipt);
        }
        let selection = self.selection(Section::History);
        self.records.history.iter().find(|row| Some(row.id) == selection)
            .or_else(|| self.records.history.first())
    }

    pub fn scene_artifact(&self) -> Option<BlockId> {
        if self.section == Section::History
            && let Some(event) = self.records.history.iter().find(|row| Some(row.id) == self.selected)
            && let Some(artifact) = self.event_artifact(event, self.scene_artifacts())
        {
            return Some(artifact.id);
        }
        let selected = self.selection(Section::Workbench);
        self.scene_artifacts().find(|row| Some(row.id) == selected)
            .or_else(|| self.scene_artifacts().next()).map(|row| row.id)
    }

    pub fn step_scene(&mut self, forward: bool) {
        if self.section == Section::History {
            if self.selected.is_none() {
                self.selected = self.records.history.first().map(|row| row.id);
            }
            self.other_selection = self.scene_artifact();
            self.step_record(forward);
            self.other_selection = self.scene_artifact();
        } else {
            let current = self.scene_artifact();
            let mut artifacts: Vec<_> = self.scene_artifacts().map(|row| row.id).collect();
            if !forward { artifacts.reverse(); }
            self.selected = artifacts.windows(2).find(|pair| Some(pair[0]) == current)
                .map(|pair| pair[1]).or(current);
        }
    }

    pub fn switch_scene_section(&mut self) {
        let current_artifact = self.scene_artifact();
        self.switch_section();
        if self.section == Section::Workbench {
            self.selected = current_artifact;
        } else {
            if let Some(row) = current_artifact.and_then(|id| self.artifact_receipt(id)) {
                self.selected = Some(row.id);
            }
            self.selected = self.selected.or_else(|| self.records.history.first().map(|row| row.id));
            self.other_selection = self.scene_artifact();
        }
    }

    fn retained(&self, section: Section, selection: Option<BlockId>) -> Option<BlockId> {
        selection.filter(|id| self.records.in_section(section).iter().any(|row| row.id == *id))
    }
}

pub(super) fn command_arg(call: &BlockSnapshot) -> Option<String> {
    crate::text::tool_input_head(call, "command", MAX_SUMMARY).or_else(|| {
        is_shell(call.tool_name.as_deref()).then(|| crate::text::tool_input_head(call, "code", MAX_SUMMARY)).flatten()
    })
}

fn is_shell(tool: Option<&str>) -> bool {
    matches!(tool, Some("shell" | "shell_readonly" | "shell_write" | "shell_read" | "kaish.exec"))
}

fn record(block: &BlockSnapshot, call: Option<&BlockSnapshot>, section: Section, content_type: ContentType, ask: Option<&str>) -> Record {
    let tool = block.tool_name.as_deref().or_else(|| call?.tool_name.as_deref());
    let command = match section {
        Section::History => call.and_then(command_arg),
        Section::Workbench => None,
    };
    let kind = match section {
        Section::Workbench => match content_type {
            ContentType::Abc => "Score",
            ContentType::Svg => "SVG",
            ContentType::Diff => "Diff",
            ContentType::Media => "Media",
            _ if block.kind == BlockKind::File => "File",
            _ => "Resource",
        },
        Section::History => match block.kind {
            BlockKind::ToolCall => "Call",
            BlockKind::ToolResult => "Result",
            BlockKind::Error => "Error",
            BlockKind::Drift => "Drift",
            BlockKind::Task => "Task",
            BlockKind::Trace => "Trace",
            _ => "Notice",
        },
    };
    let summary = block
        .content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let subject = if command.is_some() { tool } else { block.file_path.as_deref().or(tool) };
    let score_title = (section == Section::Workbench && content_type == ContentType::Abc)
        .then(|| block.content.lines().filter_map(|line| line.trim().strip_prefix("T:"))
            .map(str::trim).find(|title| !title.is_empty())).flatten();
    let title = match (subject.or(score_title), command.as_deref()) {
        (_, Some(command)) if is_shell(tool) => format!("{kind}  {command}"),
        (Some(subject), Some(command)) => format!("{kind}  {}: {command}", bounded(subject)),
        (None, Some(command)) => format!("{kind}  {command}"),
        (Some(subject), None) => format!("{kind}  {}", bounded(subject)),
        (None, _) if summary.is_empty() => kind.to_string(),
        (None, _) => format!("{kind}  {}", bounded(summary)),
    };
    let status = if ask.is_some() { Status::Waiting } else { block.status };
    let waiting;
    let status_label = if block.excluded {
        "excluded"
    } else if let Some(ask) = ask {
        waiting = format!("waiting on ask {ask}");
        &waiting
    } else {
        status.as_str()
    };
    let summary = if subject.is_some() && !summary.is_empty() {
        format!("{status_label} · {}", bounded(summary))
    } else {
        status_label.to_string()
    };
    Record {
        id: block.id,
        kind: block.kind,
        call_id: block.command_diff_parent()
            .or_else(|| call.filter(|_| block.kind == BlockKind::ToolResult).map(|call| call.id)),
        artifact_id: None,
        excluded: block.excluded,
        content_type,
        title,
        summary,
        status,
        state: if status == Status::Waiting {
            RecordState::Waiting
        } else if block.kind == BlockKind::Error || block.status == Status::Error || block.is_error {
            RecordState::Failed
        } else {
            RecordState::Normal
        },
    }
}

fn bounded(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(MAX_SUMMARY).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockSnapshotBuilder, PrincipalId};

    fn ctx(byte: u8) -> ContextId {
        ContextId::from_bytes([byte; 16])
    }

    fn id(ctx: ContextId, seq: u64) -> BlockId {
        BlockId::new(ctx, PrincipalId::from_bytes([5; 16]), seq)
    }

    fn row(id: BlockId) -> Record {
        Record {
            id,
            kind: BlockKind::Text,
            call_id: None,
            artifact_id: None,
            excluded: false,
            content_type: ContentType::Plain,
            title: String::new(),
            summary: String::new(),
            status: Status::Done,
            state: RecordState::Normal,
        }
    }

    #[test]
    fn scene_steps_over_other_artifact_types_and_links_history_to_the_patch() {
        let c = ctx(1);
        let first = id(c, 1);
        let last = id(c, 5);
        let mut ws = Workspace::default();
        ws.records.workbench = vec![
            row(id(c, 6)),
            Record { content_type: ContentType::Diff, ..row(last) },
            row(id(c, 4)),
            Record { content_type: ContentType::Diff, ..row(first) },
        ];
        ws.records.history = vec![
            Record { kind: BlockKind::ToolCall, artifact_id: Some(first), ..row(id(c, 7)) },
            row(id(c, 3)),
        ];
        assert_eq!(ws.scene_artifact(), Some(last));
        ws.step_scene(true);
        assert_eq!(ws.scene_artifact(), Some(first));
        ws.step_scene(false);
        assert_eq!(ws.scene_artifact(), Some(last));
        ws.switch_scene_section();
        assert_eq!(ws.selected, Some(id(c, 7)));
        assert_eq!(ws.scene_artifact(), Some(first));
        ws.step_scene(true);
        assert_eq!(ws.scene_artifact(), Some(first), "an unrelated event keeps the patch in place");
    }

    #[test]
    fn rail_follows_separate_creation_receipts_and_keeps_artifacts_for_unrelated_events() {
        let c = ctx(1);
        let artifact = id(c, 3);
        let call = id(c, 1);
        let receipt = id(c, 4);
        let mut ws = Workspace::default();
        ws.records.workbench = vec![row(id(c, 6)), Record { content_type: ContentType::Diff, ..row(artifact) }];
        ws.records.history = vec![
            row(id(c, 5)),
            Record { kind: BlockKind::ToolResult, call_id: Some(call), artifact_id: Some(artifact), ..row(receipt) },
            Record { kind: BlockKind::ToolCall, artifact_id: Some(artifact), ..row(call) },
        ];
        ws.selected = Some(artifact);
        ws.switch_section();
        assert_eq!(ws.selected, Some(receipt), "follow the returned block ID across the panel switch");
        ws.step(false);
        assert_eq!(ws.selected, Some(id(c, 5)));
        assert_eq!(ws.workbench_source(), Some(artifact), "keep the last patch for an unrelated event");
        ws.step(true);
        ws.step(true);
        assert_eq!(ws.workbench_source(), Some(artifact));
        ws.switch_section();
        assert_eq!(ws.selected, Some(artifact));
    }

    #[test]
    fn selection_tracks_the_id_through_inserts_and_clears_on_context_switch() {
        let context = ctx(1);
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records {
            workbench: vec![row(id(context, 2)), row(id(context, 1))],
            history: Vec::new(),
        }));
        workspace.step(true);
        workspace.step(true);
        assert_eq!(workspace.selected, Some(id(context, 1)));
        workspace.reconcile(Some(context), Some(Records {
            workbench: vec![row(id(context, 3)), row(id(context, 2)), row(id(context, 1))],
            history: Vec::new(),
        }));
        assert_eq!(workspace.selected, Some(id(context, 1)));

        let other = ctx(2);
        workspace.reconcile(Some(other), Some(Records {
            workbench: vec![row(id(other, 1))],
            history: Vec::new(),
        }));
        assert_eq!(workspace.selected, None);
    }

    #[test]
    fn switching_between_unrelated_records_keeps_the_workbench_selection() {
        let context = ctx(1);
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(Records {
            workbench: vec![row(id(context, 2)), row(id(context, 1))],
            history: vec![row(id(context, 3))],
        }));
        workspace.step(true);
        workspace.step(true);
        workspace.switch_section();
        workspace.step(true);
        workspace.switch_section();
        assert_eq!(workspace.selected, Some(id(context, 1)));
        workspace.switch_section();
        assert_eq!(workspace.selected, Some(id(context, 3)));
    }

    #[test]
    fn switch_from_a_call_selects_its_artifact_among_interleaved_results() {
        let context = ctx(1);
        let call = |seq| BlockSnapshotBuilder::new(id(context, seq), BlockKind::ToolCall).build();
        let result = |seq, call| BlockSnapshotBuilder::new(id(context, seq), BlockKind::ToolResult)
            .tool_call_id(id(context, call)).content_type(ContentType::Diff).build();
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![call(1), call(2), result(3, 2), result(4, 1)], 1).unwrap();
        let records = Records::from_mirror(&mirror, &HashMap::new(), &mut DetectedDiffs::default());
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(records));
        workspace.switch_section();
        for _ in 0..2 { workspace.step(true); }
        assert_eq!(workspace.selected, Some(id(context, 2)));
        workspace.switch_section();
        assert_eq!(workspace.selected, Some(id(context, 3)));
    }

    #[test]
    fn command_diffs_follow_the_receipt_and_keep_the_selected_file() {
        let context = ctx(1);
        let call = id(context, 1);
        let first = id(context, 2);
        let second = id(context, 3);
        let receipt = id(context, 4);
        let patch = |id, path| BlockSnapshotBuilder::new(id, BlockKind::Trace)
            .role(kaijutsu_types::Role::Asset).parent_id(call).content_type(ContentType::Diff)
            .content(format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-a\n+b\n")).build();
        let mut blocks = vec![
            BlockSnapshotBuilder::new(call, BlockKind::ToolCall).tool_name("shell").status(Status::Running).build(),
            patch(first, "first"), patch(second, "second"),
            BlockSnapshotBuilder::new(receipt, BlockKind::ToolResult)
                .tool_call_id(call).status(Status::Running).build(),
        ];
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(blocks.clone(), 1).unwrap();
        let mut workspace = Workspace::default();
        let mut diffs = DetectedDiffs::default();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &HashMap::new(), &mut diffs)));
        workspace.selected = Some(first);
        assert_eq!(workspace.history_source().map(|row| row.id), Some(call));
        assert_eq!(workspace.scene_event().map(|row| row.id), Some(call));

        blocks[0].status = Status::Done;
        blocks[3].status = Status::Done;
        mirror.apply_snapshot(blocks, 2).unwrap();
        workspace.reconcile(Some(context), Some(Records::from_mirror(&mirror, &HashMap::new(), &mut diffs)));
        assert_eq!(workspace.history_source().map(|row| row.id), Some(receipt));
        assert_eq!(workspace.scene_event().map(|row| row.id), Some(receipt));
        workspace.switch_scene_section();
        assert_eq!(workspace.selected, Some(receipt));
        assert_eq!(workspace.scene_artifact(), Some(first));
        workspace.step_scene(true);
        assert_eq!(workspace.selected, Some(call));
        assert_eq!(workspace.scene_artifact(), Some(first));
        workspace.switch_scene_section();
        assert_eq!(workspace.selected, Some(first));
    }

    #[test]
    fn creation_receipts_link_call_and_result_to_the_indexed_artifact() {
        let context = ctx(1);
        let call = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolCall)
            .tool_name("diff_block").build();
        let artifact = BlockSnapshotBuilder::new(id(context, 2), BlockKind::Text)
            .content_type(ContentType::Diff).content("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n").build();
        let result = BlockSnapshotBuilder::new(id(context, 3), BlockKind::ToolResult)
            .tool_call_id(call.id).status(Status::Done)
            .content(format!(r#"{{"block_id":"{}"}}"#, artifact.id.to_key())).build();
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![call, artifact, result], 1).unwrap();
        let records = Records::from_mirror(&mirror, &HashMap::new(), &mut DetectedDiffs::default());
        let mut workspace = Workspace::default();
        workspace.reconcile(Some(context), Some(records));
        workspace.switch_section();
        for selected in [id(context, 3), id(context, 1)] {
            workspace.selected = Some(selected);
            workspace.switch_section();
            assert_eq!(workspace.selected, Some(id(context, 2)));
            workspace.switch_section();
        }
    }

    #[test]
    fn a_single_shell_creator_uses_its_returned_block_id() {
        let c = ctx(1);
        let patch = BlockSnapshotBuilder::new(id(c, 2), BlockKind::Text).content_type(ContentType::Diff).build();
        let result = BlockSnapshotBuilder::new(id(c, 3), BlockKind::ToolResult).tool_call_id(id(c, 1))
            .status(Status::Done).exit_code(0).content(format!(r#"{{"block_id":"{}"}}"#, patch.id.to_key())).build();
        let workbench = [Record { content_type: ContentType::Diff, ..row(patch.id) }];
        for (source, linked) in [
            ("diff_block --content '--- a/orchard\n+++ b/orchard'", true),
            ("diff_block --content patch; printf forged", false),
            ("printf '%s' diff_block", false),
            ("diff_block --content patch | cat", false),
        ] {
            let input = serde_json::json!({"code": source}).to_string();
            let call = BlockSnapshotBuilder::new(id(c, 1), BlockKind::ToolCall)
                .tool_name("shell").content(input).build();
            assert_eq!(created_artifact(&result, &call, &workbench), linked.then_some(patch.id), "{source}");
        }
    }

    #[test]
    fn creation_link_rejects_unrelated_failed_and_unindexed_receipts() {
        let context = ctx(1);
        for (tool, target, failed) in [
            ("block_read", id(context, 2), false),
            ("diff_block", id(context, 2), true),
            ("diff_block", id(context, 99), false),
        ] {
            let call = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolCall).tool_name(tool).build();
            let artifact = BlockSnapshotBuilder::new(id(context, 2), BlockKind::Text)
                .content_type(ContentType::Diff).build();
            let mut result = BlockSnapshotBuilder::new(id(context, 3), BlockKind::ToolResult)
                .tool_call_id(call.id).status(Status::Done)
                .content(format!(r#"{{"block_id":"{}"}}"#, target.to_key())).build();
            result.is_error = failed;
            let mut mirror = ContextMirror::new(context);
            mirror.apply_snapshot(vec![call, artifact, result], 1).unwrap();
            let records = Records::from_mirror(&mirror, &HashMap::new(), &mut DetectedDiffs::default());
            assert!(records.history.iter().all(|row| row.artifact_id.is_none()));
        }
    }

    #[test]
    fn sections_index_real_records_in_document_order_and_bound_summaries() {
        let context = ctx(1);
        let call = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolCall)
            .tool_name("render")
            .status(Status::Done)
            .build();
        let result = BlockSnapshotBuilder::new(id(context, 2), BlockKind::ToolResult)
            .tool_call_id(call.id)
            .content_type(ContentType::Svg)
            .content("x".repeat(400))
            .status(Status::Done)
            .build();
        let draft = BlockSnapshotBuilder::new(id(context, 3), BlockKind::Text)
            .content_type(ContentType::Diff)
            .status(Status::Draft)
            .build();
        let (call_id, result_id) = (call.id, result.id);
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![call, result, draft], 1).unwrap();
        let records = Records::from_mirror(&mirror, &HashMap::new(), &mut DetectedDiffs::default());

        assert_eq!(records.workbench.len(), 1);
        assert_eq!(records.workbench[0].id, result_id);
        assert_eq!(records.workbench[0].title, "SVG  render");
        assert!(records.workbench[0].summary.ends_with('…'));
        assert_eq!(records.history.iter().map(|r| r.id).collect::<Vec<_>>(), [result_id, call_id]);
    }

    #[test]
    fn history_names_commands_on_calls_and_linked_results() {
        let context = ctx(1);
        let call = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolCall)
            .tool_name("shell")
            .tool_input(r#"{"command":"  kj context list\nnext"}"#)
            .file_path("/tmp/irrelevant")
            .build();
        let result = BlockSnapshotBuilder::new(id(context, 2), BlockKind::ToolResult)
            .tool_call_id(call.id)
            .content("some output")
            .build();
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![call, result], 1).unwrap();
        let records = Records::from_mirror(&mirror, &HashMap::new(), &mut DetectedDiffs::default());
        assert_eq!(records.history[0].title, "Result  kj context list");
        assert_eq!(records.history[1].title, "Call  kj context list");
        assert!(records.history[0].summary.contains("some output"));
    }

    #[test]
    fn history_links_interleaved_calls_and_handles_missing_input() {
        let context = ctx(1);
        let call = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolCall)
            .tool_name("shell").tool_input(r#"{"code":"echo desktop"}"#).build();
        let other = BlockSnapshotBuilder::new(id(context, 2), BlockKind::ToolCall)
            .tool_name("shell_write").tool_input("bad json").build();
        let result = BlockSnapshotBuilder::new(id(context, 3), BlockKind::ToolResult)
            .tool_call_id(call.id).build();
        let orphan = BlockSnapshotBuilder::new(id(context, 4), BlockKind::ToolResult)
            .tool_call_id(id(context, 99)).build();
        let mut mirror = ContextMirror::new(context);
        mirror.apply_snapshot(vec![call, other, result, orphan], 1).unwrap();
        let records = Records::from_mirror(&mirror, &HashMap::new(), &mut DetectedDiffs::default());
        let titles: Vec<_> = records.history.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["Result", "Result  echo desktop", "Call  shell_write", "Call  echo desktop"]);
    }

    #[test]
    fn approval_wait_is_not_rendered_as_a_failure() {
        let context = ctx(1);
        let mut block = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolResult)
            .status(Status::Waiting)
            .build();
        block.is_error = true;
        let waiting = record(&block, None, Section::History, block.content_type, None);
        assert_eq!(waiting.state, RecordState::Waiting);
        assert_eq!(waiting.summary, "waiting");
    }
}
