//! `FileToolsServer` — virtual MCP server exposing file tools (read, edit,
//! write, glob, grep) through the broker.
//!
//! Phase 1 delegates bodies to the existing `file_tools` engines. Params
//! structs derive `schemars::JsonSchema` so the MCP-exposed schemas come from
//! the type system (D-17). `WorkspaceGuard` (when configured) enforces at
//! `call_tool` entry based on `CallContext::cwd`.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::file_tools::{
    EditEngine, FileDocumentCache, GlobEngine, GrepEngine, ReadEngine, WorkspaceGuard, WriteEngine,
};

use crate::vfs::MountTable;

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
use super::adapter::{from_exec_result, to_exec_context};

// ── Typed Params (schemars-derived) ────────────────────────────────────────

/// Parameters for the `read` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// File path to read (relative to VFS root).
    pub path: String,
    /// Start line (0-indexed). Omit to read from beginning.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum number of lines to return. Omit for all lines.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `edit` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// File path to edit.
    pub path: String,
    /// Exact string to find and replace.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace all occurrences (default: false).
    #[serde(default)]
    pub replace_all: bool,
}

/// Parameters for the `write` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteParams {
    /// File path to write.
    pub path: String,
    /// File content.
    pub content: String,
}

/// Parameters for the `glob` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobParams {
    /// Glob pattern (e.g., `**/*.rs`).
    pub pattern: String,
    /// Directory to search (defaults to VFS root).
    #[serde(default)]
    pub path: Option<String>,
}

/// Parameters for the `grep` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepParams {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Optional directory to restrict search.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional glob filter for filenames.
    #[serde(default)]
    pub glob: Option<String>,
    /// Lines of context before/after each match.
    #[serde(default)]
    pub context_lines: u32,
}

// ── Server ─────────────────────────────────────────────────────────────────

pub struct FileToolsServer {
    instance_id: InstanceId,
    read: ReadEngine,
    edit: EditEngine,
    write: WriteEngine,
    glob: GlobEngine,
    grep: GrepEngine,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl FileToolsServer {
    pub const INSTANCE: &'static str = "builtin.file";

    pub fn new(
        cache: Arc<FileDocumentCache>,
        vfs: Arc<MountTable>,
        guard: Option<WorkspaceGuard>,
    ) -> Self {
        let (read, edit, write, glob_engine, grep) = match guard {
            Some(guard) => (
                ReadEngine::new(cache.clone()).with_guard(guard.clone()),
                EditEngine::new(cache.clone()).with_guard(guard.clone()),
                WriteEngine::new(cache.clone()).with_guard(guard.clone()),
                GlobEngine::new(vfs.clone()).with_guard(guard.clone()),
                GrepEngine::new(cache.clone(), vfs.clone()).with_guard(guard),
            ),
            None => (
                ReadEngine::new(cache.clone()),
                EditEngine::new(cache.clone()),
                WriteEngine::new(cache.clone()),
                GlobEngine::new(vfs.clone()),
                GrepEngine::new(cache.clone(), vfs),
            ),
        };

        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            read,
            edit,
            write,
            glob: glob_engine,
            grep,
            notif_tx,
        }
    }
}

fn tool_def<P: JsonSchema>(
    instance: &InstanceId,
    name: &str,
    description: &str,
) -> McpResult<KernelTool> {
    let schema = schemars::schema_for!(P);
    Ok(KernelTool {
        instance: instance.clone(),
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
    })
}

#[async_trait]
impl McpServerLike for FileToolsServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<ReadParams>(&self.instance_id, "read", self.read.description())?,
            tool_def::<EditParams>(&self.instance_id, "edit", self.edit.description())?,
            tool_def::<WriteParams>(&self.instance_id, "write", self.write.description())?,
            tool_def::<GlobParams>(&self.instance_id, "glob", self.glob.description())?,
            tool_def::<GrepParams>(&self.instance_id, "grep", self.grep.description())?,
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let tool_ctx = to_exec_context(ctx);
        let args_json = params.arguments.to_string();

        let exec = match params.tool.as_str() {
            "read" => {
                // Validate shape against the typed Params.
                let _: ReadParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                self.read
                    .execute(&args_json, &tool_ctx)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?
            }
            "edit" => {
                let _: EditParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                self.edit
                    .execute(&args_json, &tool_ctx)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?
            }
            "write" => {
                let _: WriteParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                self.write
                    .execute(&args_json, &tool_ctx)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?
            }
            "glob" => {
                let _: GlobParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                self.glob
                    .execute(&args_json, &tool_ctx)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?
            }
            "grep" => {
                let _: GrepParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                self.grep
                    .execute(&args_json, &tool_ctx)
                    .await
                    .map_err(|e| McpError::Protocol(e.to_string()))?
            }
            other => {
                return Err(McpError::ToolNotFound {
                    instance: self.instance_id.clone(),
                    tool: other.to_string(),
                });
            }
        };

        Ok(from_exec_result(exec))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::{shared_block_store_with_db, DocumentKind};
    use crate::file_tools::FileDocumentCache;
    use crate::kernel_db::KernelDb;
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use crate::vfs::MountTable;
    use kaijutsu_types::{KernelId, PrincipalId};

    #[tokio::test]
    async fn glob_via_broker() {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let kernel_id = KernelId::new();
        let ws_id = db
            .lock()
            .get_or_create_default_workspace(kernel_id, creator)
            .unwrap();
        let store = shared_block_store_with_db(db, kernel_id, ws_id, creator);
        let _ = (&store, DocumentKind::Code); // keep imports meaningful

        let vfs = Arc::new(MountTable::new());
        let cache = Arc::new(FileDocumentCache::new(store, vfs.clone()));

        let server = Arc::new(FileToolsServer::new(cache, vfs, None));
        let broker = Broker::new();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let ctx = CallContext::test();
        let result = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(FileToolsServer::INSTANCE),
                    tool: "glob".to_string(),
                    arguments: serde_json::json!({ "pattern": "**/*.nonexistent" }),
                },
                &ctx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();

        // Empty result is fine — the test asserts the pipeline reaches the
        // engine and returns a non-error KernelToolResult shape.
        assert!(!result.is_error, "glob should not error: {:?}", result.content);
        assert!(matches!(result.content.first(), Some(ToolContent::Text(_))));
    }
}

