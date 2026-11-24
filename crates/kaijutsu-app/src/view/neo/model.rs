use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_client::ContextMirror;
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, Status};

use crate::view::surface::chrome::pending_tool_ask;

use super::projection::NeoProjection;

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
    pub title: String,
    pub summary: String,
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
    pub(super) fn from_mirror(mirror: &ContextMirror, waiting_on: &HashMap<BlockId, String>) -> Self {
        let blocks = mirror.blocks();
        let projection = NeoProjection::from_blocks(&blocks[blocks.len().saturating_sub(LOOKBACK)..]);
        let collect = |events: Vec<super::projection::NeoEvent>, section| {
            events.into_iter().rev().take(MAX_RECORDS).filter_map(|event| {
                let block = mirror.block(&event.id)?;
                let tool = block.tool_name.as_deref().or_else(|| {
                    let call = mirror.block(&block.tool_call_id?)?;
                    (call.kind == BlockKind::ToolCall).then_some(call.tool_name.as_deref()).flatten()
                });
                Some(record(block, tool, section, pending_tool_ask(block, waiting_on)))
            }).collect()
        };
        Self {
            workbench: collect(projection.artifacts, Section::Workbench),
            history: collect(projection.history, Section::History),
        }
    }

    pub(super) fn in_section(&self, section: Section) -> &[Record] {
        match section {
            Section::Workbench => &self.workbench,
            Section::History => &self.history,
        }
    }
}

#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub(super) struct Workspace {
    pub context: Option<ContextId>,
    pub ready: bool,
    pub section: Section,
    pub selected: Option<BlockId>,
    pub records: Records,
}

impl Workspace {
    pub fn reconcile(&mut self, context: Option<ContextId>, records: Option<Records>) {
        if self.context != context {
            self.context = context;
            self.section = Section::default();
            self.selected = None;
        }
        self.ready = records.is_some();
        self.records = records.unwrap_or_default();
        self.retain_selection();
    }

    pub fn switch_section(&mut self) {
        self.section = self.section.other();
        self.retain_selection();
    }

    pub fn step(&mut self, forward: bool) {
        let records = self.records.in_section(self.section);
        let current = records.iter().position(|row| Some(row.id) == self.selected);
        let next = match current {
            Some(i) if forward => (i + 1).min(records.len() - 1),
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.selected = records.get(next).map(|row| row.id);
    }

    fn retain_selection(&mut self) {
        self.selected = self.selected.filter(|id| {
            self.records.in_section(self.section).iter().any(|row| row.id == *id)
        });
    }
}

fn record(block: &BlockSnapshot, tool: Option<&str>, section: Section, ask: Option<&str>) -> Record {
    let kind = match section {
        Section::Workbench => match block.content_type {
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
    let subject = block.file_path.as_deref().or(tool);
    let title = match subject {
        Some(subject) => format!("{kind}  {}", bounded(subject)),
        None if summary.is_empty() => kind.to_string(),
        None => format!("{kind}  {}", bounded(summary)),
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
        title,
        summary,
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
            title: String::new(),
            summary: String::new(),
            state: RecordState::Normal,
        }
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
        let records = Records::from_mirror(&mirror, &HashMap::new());

        assert_eq!(records.workbench.len(), 1);
        assert_eq!(records.workbench[0].id, result_id);
        assert_eq!(records.workbench[0].title, "SVG  render");
        assert!(records.workbench[0].summary.ends_with('…'));
        assert_eq!(records.history.iter().map(|r| r.id).collect::<Vec<_>>(), [result_id, call_id]);
    }

    #[test]
    fn approval_wait_is_not_rendered_as_a_failure() {
        let context = ctx(1);
        let mut block = BlockSnapshotBuilder::new(id(context, 1), BlockKind::ToolResult)
            .status(Status::Waiting)
            .build();
        block.is_error = true;
        let waiting = record(&block, Some("kaish"), Section::History, None);
        assert_eq!(waiting.state, RecordState::Waiting);
        assert_eq!(waiting.summary, "waiting");
    }
}
