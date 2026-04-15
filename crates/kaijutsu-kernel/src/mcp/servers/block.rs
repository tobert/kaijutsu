//! `BlockToolsServer` — virtual MCP server exposing block and content-creation
//! tools (D-30). Phase 1 delegates bodies to the existing `block_tools`
//! engines; schemas come from `schemars` via the existing Params structs.
//!
//! Tools exposed (13):
//!   structural: block_create, block_append, block_edit, block_splice,
//!               block_read, block_search, block_list, block_status
//!   cross-block: kernel_search
//!   content: svg_block, abc_block, img_block, img_block_from_path

use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_cas::FileStore;
use schemars::JsonSchema;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::block_store::SharedBlockStore;
use crate::block_tools::{
    AbcBlockEngine, BlockAppendEngine, BlockAppendParams, BlockCreateEngine, BlockCreateParams,
    BlockEditEngine, BlockEditParams, BlockListEngine, BlockListParams, BlockReadEngine,
    BlockReadParams, BlockSearchEngine, BlockSearchParams, BlockSpliceEngine, BlockSpliceParams,
    BlockStatusEngine, BlockStatusParams, ImgBlockEngine, ImgBlockFromPathEngine,
    KernelSearchEngine, KernelSearchParams, SvgBlockEngine,
};
use crate::block_tools::content_engines::{
    AbcBlockParams, ImgBlockFromPathParams, ImgBlockParams, SvgBlockParams,
};


use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
use super::adapter::{from_exec_result, to_exec_context};

pub struct BlockToolsServer {
    instance_id: InstanceId,
    // Structural block tools.
    create: BlockCreateEngine,
    append: BlockAppendEngine,
    edit: BlockEditEngine,
    splice: BlockSpliceEngine,
    read: BlockReadEngine,
    search: BlockSearchEngine,
    list: BlockListEngine,
    status: BlockStatusEngine,
    kernel_search: KernelSearchEngine,
    // Content-type tools.
    svg: SvgBlockEngine,
    abc: AbcBlockEngine,
    img: ImgBlockEngine,
    img_from_path: ImgBlockFromPathEngine,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BlockToolsServer {
    pub const INSTANCE: &'static str = "builtin.block";

    pub fn new(documents: SharedBlockStore, cas: Arc<FileStore>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            create: BlockCreateEngine::new(documents.clone()),
            append: BlockAppendEngine::new(documents.clone()),
            edit: BlockEditEngine::new(documents.clone()),
            splice: BlockSpliceEngine::new(documents.clone()),
            read: BlockReadEngine::new(documents.clone()),
            search: BlockSearchEngine::new(documents.clone()),
            list: BlockListEngine::new(documents.clone()),
            status: BlockStatusEngine::new(documents.clone()),
            kernel_search: KernelSearchEngine::new(documents.clone()),
            svg: SvgBlockEngine::new(documents.clone()),
            abc: AbcBlockEngine::new(documents.clone()),
            img: ImgBlockEngine::new(documents.clone()),
            img_from_path: ImgBlockFromPathEngine::new(documents, cas),
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
impl McpServerLike for BlockToolsServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<BlockCreateParams>(&self.instance_id, "block_create", self.create.description())?,
            tool_def::<BlockAppendParams>(&self.instance_id, "block_append", self.append.description())?,
            tool_def::<BlockEditParams>(&self.instance_id, "block_edit", self.edit.description())?,
            tool_def::<BlockSpliceParams>(&self.instance_id, "block_splice", self.splice.description())?,
            tool_def::<BlockReadParams>(&self.instance_id, "block_read", self.read.description())?,
            tool_def::<BlockSearchParams>(&self.instance_id, "block_search", self.search.description())?,
            tool_def::<BlockListParams>(&self.instance_id, "block_list", self.list.description())?,
            tool_def::<BlockStatusParams>(&self.instance_id, "block_status", self.status.description())?,
            tool_def::<KernelSearchParams>(&self.instance_id, "kernel_search", self.kernel_search.description())?,
            tool_def::<SvgBlockParams>(&self.instance_id, "svg_block", self.svg.description())?,
            tool_def::<AbcBlockParams>(&self.instance_id, "abc_block", self.abc.description())?,
            tool_def::<ImgBlockParams>(&self.instance_id, "img_block", self.img.description())?,
            tool_def::<ImgBlockFromPathParams>(&self.instance_id, "img_block_from_path", self.img_from_path.description())?,
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
            "block_create" => {
                let _: BlockCreateParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.create.execute(&args_json, &tool_ctx).await
            }
            "block_append" => {
                let _: BlockAppendParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.append.execute(&args_json, &tool_ctx).await
            }
            "block_edit" => {
                let _: BlockEditParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.edit.execute(&args_json, &tool_ctx).await
            }
            "block_splice" => {
                let _: BlockSpliceParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.splice.execute(&args_json, &tool_ctx).await
            }
            "block_read" => {
                let _: BlockReadParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.read.execute(&args_json, &tool_ctx).await
            }
            "block_search" => {
                let _: BlockSearchParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.search.execute(&args_json, &tool_ctx).await
            }
            "block_list" => {
                let _: BlockListParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.list.execute(&args_json, &tool_ctx).await
            }
            "block_status" => {
                let _: BlockStatusParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.status.execute(&args_json, &tool_ctx).await
            }
            "kernel_search" => {
                let _: KernelSearchParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.kernel_search.execute(&args_json, &tool_ctx).await
            }
            "svg_block" => {
                let _: SvgBlockParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.svg.execute(&args_json, &tool_ctx).await
            }
            "abc_block" => {
                let _: AbcBlockParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.abc.execute(&args_json, &tool_ctx).await
            }
            "img_block" => {
                let _: ImgBlockParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.img.execute(&args_json, &tool_ctx).await
            }
            "img_block_from_path" => {
                let _: ImgBlockFromPathParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                self.img_from_path.execute(&args_json, &tool_ctx).await
            }
            other => {
                return Err(McpError::ToolNotFound {
                    instance: self.instance_id.clone(),
                    tool: other.to_string(),
                });
            }
        }
        .map_err(|e| McpError::Protocol(e.to_string()))?;

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
    use crate::kernel_db::{DocumentRow, KernelDb};
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use kaijutsu_cas::FileStore;
    use kaijutsu_types::{now_millis, KernelId, PrincipalId};

    async fn setup() -> (Arc<Broker>, CallContext) {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let kernel_id = KernelId::new();
        let ws_id = {
            let g = db.lock();
            g.get_or_create_default_workspace(kernel_id, creator).unwrap()
        };
        let store = shared_block_store_with_db(db.clone(), kernel_id, ws_id, creator);

        let mut ctx = CallContext::test();
        ctx.kernel_id = kernel_id;
        ctx.principal_id = creator;
        {
            let g = db.lock();
            g.insert_document(&DocumentRow {
                document_id: ctx.context_id,
                kernel_id,
                workspace_id: ws_id,
                doc_kind: DocumentKind::Code,
                language: None,
                path: None,
                created_at: now_millis() as i64,
                created_by: creator,
            })
            .unwrap();
        }
        store
            .create_document(ctx.context_id, DocumentKind::Code, None)
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(FileStore::at_path(tmp.path().join("cas")));
        let server = Arc::new(BlockToolsServer::new(store, cas));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        std::mem::forget(tmp); // keep the dir alive for the test duration
        (broker, ctx)
    }

    #[tokio::test]
    async fn block_create_via_broker() {
        let (broker, ctx) = setup().await;
        let result = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(BlockToolsServer::INSTANCE),
                    tool: "block_create".to_string(),
                    arguments: serde_json::json!({
                        "role": "user",
                        "kind": "text",
                        "content": "hello from mcp broker",
                    }),
                },
                &ctx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!result.is_error, "unexpected error: {:?}", result.content);
        assert!(matches!(result.content.first(), Some(ToolContent::Text(_))));
    }

    #[tokio::test]
    async fn list_tools_exposes_all_thirteen() {
        let (broker, ctx) = setup().await;
        let visible = {
            let mut binding = crate::mcp::ContextToolBinding::new();
            binding.allow(InstanceId::new(BlockToolsServer::INSTANCE));
            broker.set_binding(ctx.context_id, binding).await;
            broker.list_visible_tools(ctx.context_id, &ctx).await.unwrap()
        };
        let names: Vec<_> = visible.iter().map(|(n, _)| n.as_str()).collect();
        for expected in [
            "block_create",
            "block_append",
            "block_edit",
            "block_splice",
            "block_read",
            "block_search",
            "block_list",
            "block_status",
            "kernel_search",
            "svg_block",
            "abc_block",
            "img_block",
            "img_block_from_path",
        ] {
            assert!(names.contains(&expected), "missing {}", expected);
        }
    }
}
