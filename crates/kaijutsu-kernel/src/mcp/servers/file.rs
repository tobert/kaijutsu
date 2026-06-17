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
    /// File path. Relative paths resolve against the context cwd.
    pub path: String,
    /// 1-indexed line to start at (matches the line numbers in the output and
    /// in grep results). Omit to start at line 1.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum number of lines to return. Omit to use the default cap (2000);
    /// the output notes when the window is partial.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `edit` tool. Two addressing modes — pass exactly one of
/// `old_string` (string mode) or `anchor` (hashline mode).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// File path to edit.
    pub path: String,
    /// String mode: exact substring to find and replace (whitespace-exact).
    /// Mutually exclusive with `anchor`.
    #[serde(default)]
    pub old_string: Option<String>,
    /// Replacement text. In hashline mode this is the full new content for the
    /// anchored line(s); an empty string deletes them.
    pub new_string: String,
    /// String mode only: replace every occurrence instead of requiring a unique
    /// match (default: false).
    #[serde(default)]
    pub replace_all: bool,
    /// Hashline mode: address a line or range by the `N:hash` anchors that
    /// `read` prints — `42:a3f1` for one line, `42:a3f1..45:0e9c` for an
    /// inclusive range. The hash is reverified before writing, so a stale edit
    /// fails loud instead of corrupting. Mutually exclusive with `old_string`.
    #[serde(default)]
    pub anchor: Option<String>,
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
    use crate::block_store::{shared_block_store, shared_block_store_with_db, DocumentKind};
    use crate::file_tools::FileDocumentCache;
    use crate::kernel_db::KernelDb;
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use crate::vfs::backends::MemoryBackend;
    use crate::vfs::MountTable;
    use kaijutsu_types::PrincipalId;

    /// Build a broker fronting a `builtin.file` server over a MemoryBackend at
    /// /tmp, pre-seeded with `content` at `path`. Returns the broker and a handle
    /// to the same cache so tests can assert the post-edit content directly.
    async fn broker_with_file(path: &str, content: &str) -> (Arc<Broker>, Arc<FileDocumentCache>) {
        let blocks = shared_block_store(PrincipalId::system());
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/tmp", MemoryBackend::new()).await;
        let cache = Arc::new(FileDocumentCache::new(blocks, vfs.clone()));
        cache.create_or_replace(path, content).await.unwrap();

        let server = Arc::new(FileToolsServer::new(cache.clone(), vfs, None));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        (broker, cache)
    }

    async fn call(broker: &Broker, tool: &str, args: serde_json::Value) -> KernelToolResult {
        broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(FileToolsServer::INSTANCE),
                    tool: tool.to_string(),
                    arguments: args,
                },
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    fn text_of(r: &KernelToolResult) -> String {
        match r.content.first() {
            Some(ToolContent::Text(s)) => s.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// End-to-end through the broker: the docs/issues.md corruption class — a
    /// substring edit on a file with multibyte content *before* the match. The
    /// byte/char bug would splice at the wrong place; here the file must come out
    /// exactly right.
    #[tokio::test]
    async fn edit_multibyte_string_mode_round_trips_via_broker() {
        let path = "/tmp/issues.md";
        let content = "# 改善\n\n- α bullet\n- target line →\n";
        let (broker, cache) = broker_with_file(path, content).await;

        let res = call(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "old_string": "target line →",
                "new_string": "REPLACED",
            }),
        )
        .await;
        assert!(!res.is_error, "edit failed: {}", text_of(&res));
        assert_eq!(
            cache.read_content(path).await.unwrap(),
            "# 改善\n\n- α bullet\n- REPLACED\n"
        );
    }

    /// End-to-end hashline round trip: read to learn a line's `N:hash`, then edit
    /// by anchor. Exercises read's annotation and edit's anchor path together.
    #[tokio::test]
    async fn edit_anchor_mode_round_trips_via_broker() {
        let path = "/tmp/doc.md";
        let content = "# 改善\n\n- α bullet\n- target line →\n";
        let (broker, cache) = broker_with_file(path, content).await;

        let read = call(&broker, "read", serde_json::json!({ "path": path })).await;
        let rendered = text_of(&read);
        // The line we want shows as `   4:hash→ - target line →`. Take the text
        // before the FIRST `→` (the separator) as the `N:hash` anchor.
        let line = rendered
            .lines()
            .find(|l| l.contains("target"))
            .expect("read output should contain the target line");
        let anchor = line.split_once('→').unwrap().0.trim().to_string();

        let res = call(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "anchor": anchor,
                "new_string": "- done",
            }),
        )
        .await;
        assert!(!res.is_error, "anchor edit failed: {}", text_of(&res));
        assert_eq!(
            cache.read_content(path).await.unwrap(),
            "# 改善\n\n- α bullet\n- done\n"
        );
    }

    /// A stale anchor (wrong hash) must fail loud, not splice the wrong line.
    #[tokio::test]
    async fn edit_stale_anchor_fails_loud_via_broker() {
        let path = "/tmp/stale.md";
        let (broker, cache) = broker_with_file(path, "one\ntwo\nthree\n").await;

        let res = call(
            &broker,
            "edit",
            serde_json::json!({
                "path": path,
                "anchor": "2:0000",
                "new_string": "X",
            }),
        )
        .await;
        assert!(res.is_error, "stale anchor should fail");
        assert!(text_of(&res).contains("stale"), "got: {}", text_of(&res));
        // File untouched.
        assert_eq!(cache.read_content(path).await.unwrap(), "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn glob_via_broker() {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = db
            .lock()
            .get_or_create_default_workspace(creator)
            .unwrap();
        let store = shared_block_store_with_db(db, ws_id, creator);
        let _ = (&store, DocumentKind::Code); // keep imports meaningful

        let vfs = Arc::new(MountTable::new());
        let cache = Arc::new(FileDocumentCache::new(store, vfs.clone()));

        let server = Arc::new(FileToolsServer::new(cache, vfs, None));
        let broker = Arc::new(Broker::new());
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

