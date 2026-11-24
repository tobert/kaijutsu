//! A document-order projection of conversation blocks for Neo.

use std::collections::HashSet;

use kaijutsu_types::block::{BlockId, BlockKind, BlockSnapshot, ContentType};

#[derive(Debug, PartialEq)]
pub struct NeoEvent {
    pub id: BlockId,
    pub created_at: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct NeoToolEvent {
    pub event: NeoEvent,
    pub tool_call_id: Option<BlockId>,
    pub paired_call: Option<BlockId>,
}

#[derive(Debug, Default, PartialEq)]
pub struct NeoProjection {
    pub dialogue: Vec<NeoEvent>,
    pub tools: Vec<NeoToolEvent>,
    pub artifacts: Vec<NeoEvent>,
}

impl NeoProjection {
    pub fn from_blocks<'a>(blocks: impl IntoIterator<Item = &'a BlockSnapshot>) -> Self {
        let blocks: Vec<_> = blocks.into_iter().collect();
        let calls: HashSet<_> = blocks.iter()
            .filter(|block| block.kind == BlockKind::ToolCall)
            .map(|block| block.id)
            .collect();
        let mut projection = Self::default();
        for block in blocks {
            let event = || NeoEvent {
                id: block.id,
                created_at: (block.created_at != 0).then_some(block.created_at),
            };
            let artifact = matches!(block.content_type,
                ContentType::Svg | ContentType::Abc | ContentType::Diff | ContentType::Media);
            let tool = matches!(block.kind, BlockKind::ToolCall | BlockKind::ToolResult);
            if artifact {
                projection.artifacts.push(event());
            }
            if tool {
                let tool_call_id = if block.kind == BlockKind::ToolResult {
                    block.tool_call_id
                } else {
                    None
                };
                projection.tools.push(NeoToolEvent {
                    event: event(),
                    tool_call_id,
                    paired_call: tool_call_id.filter(|id| calls.contains(id)),
                });
            } else if !artifact {
                projection.dialogue.push(event());
            }
        }
        projection
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::block::{BlockKind, BlockSnapshotBuilder, ContentType};
    use kaijutsu_types::{ContextId, PrincipalId};

    fn id(seq: u64) -> BlockId {
        BlockId::new(ContextId::from_bytes([1; 16]), PrincipalId::from_bytes([2; 16]), seq)
    }

    fn block(seq: u64, kind: BlockKind, content_type: ContentType, created_at: u64) -> BlockSnapshot {
        BlockSnapshotBuilder::new(id(seq), kind)
            .content_type(content_type)
            .created_at(created_at)
            .build()
    }

    #[test]
    fn projects_order_and_only_explicit_rich_content() {
        let blocks = vec![
            block(1, BlockKind::Text, ContentType::Plain, 200),
            block(2, BlockKind::Text, ContentType::Markdown, 100),
            block(3, BlockKind::Thinking, ContentType::Plain, 0),
            block(4, BlockKind::Text, ContentType::Svg, 300),
            block(5, BlockKind::Text, ContentType::Abc, 400),
            block(6, BlockKind::Text, ContentType::Diff, 500),
            block(7, BlockKind::Text, ContentType::Media, 600),
        ];
        let p = NeoProjection::from_blocks(&blocks);
        assert_eq!(p.dialogue.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id(1), id(2), id(3)]);
        assert_eq!(p.dialogue.iter().map(|e| e.created_at).collect::<Vec<_>>(), vec![Some(200), Some(100), None]);
        assert_eq!(p.artifacts.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id(4), id(5), id(6), id(7)]);
        assert!(p.tools.is_empty());
    }

    #[test]
    fn interleaved_calls_results_and_orphans_keep_source_order() {
        let call = block(1, BlockKind::ToolCall, ContentType::Plain, 0);
        let second = block(2, BlockKind::ToolCall, ContentType::Plain, 50);
        let result = BlockSnapshotBuilder::new(id(3), BlockKind::ToolResult).tool_call_id(id(1)).created_at(60).build();
        let orphan = BlockSnapshotBuilder::new(id(4), BlockKind::ToolResult).tool_call_id(id(99)).created_at(0).build();
        let standalone = block(5, BlockKind::ToolResult, ContentType::Plain, 80);
        let rich_result = BlockSnapshotBuilder::new(id(6), BlockKind::ToolResult).tool_call_id(id(2)).content_type(ContentType::Diff).created_at(90).build();
        let blocks = vec![call, second, result, orphan, standalone, rich_result];
        let p = NeoProjection::from_blocks(&blocks);
        assert_eq!(p.tools.iter().map(|e| e.event.id).collect::<Vec<_>>(), vec![id(1), id(2), id(3), id(4), id(5), id(6)]);
        assert_eq!(p.tools.iter().map(|e| e.paired_call).collect::<Vec<_>>(), vec![None, None, Some(id(1)), None, None, Some(id(2))]);
        assert_eq!(p.tools[3].tool_call_id, Some(id(99)));
        assert_eq!(p.tools[3].event.created_at, None);
        assert_eq!(p.artifacts.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id(6)]);
    }

    #[test]
    fn result_does_not_pair_with_a_non_call_block() {
        let text = block(1, BlockKind::Text, ContentType::Plain, 1);
        let result = BlockSnapshotBuilder::new(id(2), BlockKind::ToolResult).tool_call_id(id(1)).build();
        let blocks = [text, result];
        let p = NeoProjection::from_blocks(&blocks);
        assert_eq!(p.tools[0].tool_call_id, Some(id(1)));
        assert_eq!(p.tools[0].paired_call, None);
    }
}
