//! `BuiltinToolSearchServer` — keyword search across visible tools (M3-D2).
//!
//! One tool, `tool_search { query, limit? }`, runs a substring scan over
//! the calling context's visible tools (name + description) and returns
//! the highest-scoring matches. Vector search (kaijutsu-index) is the v2
//! upgrade, gated on the hnsw_rs reverse-edge fix.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};

const DEFAULT_LIMIT: u32 = 20;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolSearchParams {
    /// Substring or keyword to search for. Lowercased before match.
    pub query: String,
    /// Max results to return (default 20). Pass 0 for the default.
    #[serde(default)]
    pub limit: Option<u32>,
}

pub struct BuiltinToolSearchServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinToolSearchServer {
    pub const INSTANCE: &'static str = "builtin.tool_search";

    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            broker,
            notif_tx,
        }
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }
}

/// Score a tool against a lowercased query. Name matches are worth 3,
/// description matches are worth 1. Returns `None` if neither field
/// matches — callers filter those out.
pub(crate) fn score_tool(query_lower: &str, name: &str, description: Option<&str>) -> Option<u32> {
    if query_lower.is_empty() {
        return None;
    }
    let mut s: u32 = 0;
    if name.to_lowercase().contains(query_lower) {
        s += 3;
    }
    if let Some(d) = description
        && d.to_lowercase().contains(query_lower)
    {
        s += 1;
    }
    if s == 0 { None } else { Some(s) }
}

#[async_trait]
impl McpServerLike for BuiltinToolSearchServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let schema = schemars::schema_for!(ToolSearchParams);
        Ok(vec![KernelTool {
            instance: self.instance_id.clone(),
            name: "tool_search".to_string(),
            description: Some(
                "Find tools whose name or description matches a substring \
                 query. Returns highest-scoring matches across the calling \
                 context's visible tools."
                    .to_string(),
            ),
            input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
        }])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool != "tool_search" {
            return Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: params.tool,
            });
        }
        let parsed: ToolSearchParams =
            serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
        let query = parsed.query.trim().to_lowercase();
        let limit = match parsed.limit {
            Some(0) | None => DEFAULT_LIMIT,
            Some(n) => n,
        } as usize;

        // Honor binding + ListTools hooks via list_visible_tools — search
        // sees exactly what the model would see when calling tools.
        let broker = self.broker()?;
        let visible = broker.list_visible_tools(ctx.context_id, ctx).await?;

        // visible is Vec<(resolved_name, KernelTool)> — the resolved name is
        // what the model would call (potentially `instance.tool` if unique
        // resolution disambiguated). Score against the resolved name +
        // tool description so search is consistent with invocation.
        let mut scored: Vec<(u32, &(String, KernelTool))> = visible
            .iter()
            .filter_map(|entry| {
                let (resolved_name, tool) = entry;
                score_tool(&query, resolved_name, tool.description.as_deref())
                    .map(|s| (s, entry))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.0.cmp(&b.1.0)));

        let results: Vec<serde_json::Value> = scored
            .into_iter()
            .take(limit)
            .map(|(score, (resolved_name, t))| {
                serde_json::json!({
                    "instance": t.instance.as_str(),
                    "name": resolved_name,
                    "tool": t.name,
                    "description": t.description,
                    "score": score,
                })
            })
            .collect();

        let payload = serde_json::json!({
            "query": parsed.query,
            "matches": results,
        });
        Ok(KernelToolResult {
            is_error: false,
            content: vec![],
            structured: Some(payload),
        })
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_tool_prefers_name_matches() {
        let s_name = score_tool("read", "file_read", Some("Read a file from disk")).unwrap();
        let s_desc = score_tool("read", "block_create", Some("Read the blocks once.")).unwrap();
        assert!(
            s_name > s_desc,
            "name match should outrank description match (name={s_name}, desc={s_desc})"
        );
    }

    #[test]
    fn score_tool_no_match_returns_none() {
        assert!(score_tool("xyzzy", "file_read", Some("Read a file")).is_none());
    }

    #[test]
    fn score_tool_empty_query_returns_none() {
        // Empty query shouldn't match every tool — that defeats the
        // purpose of a search.
        assert!(score_tool("", "file_read", Some("Read a file")).is_none());
    }

    #[test]
    fn score_tool_is_case_insensitive() {
        assert!(score_tool("read", "FILE_READ", None).is_some());
    }

    #[test]
    fn score_tool_handles_missing_description() {
        // Name-only match is still a hit even if description is None.
        let s = score_tool("read", "file_read", None).unwrap();
        assert_eq!(s, 3);
    }
}
