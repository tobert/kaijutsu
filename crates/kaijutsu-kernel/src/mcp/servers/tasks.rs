//! `BuiltinTasksServer` — virtual MCP server exposing task/plan grooming
//! tools (household-agent arc; `docs/tasks.md`).
//!
//! A sibling of `BlockToolsServer` (`block.rs`), not an extension of it: task
//! grooming is a distinct, curated surface (create/update/complete/cancel/
//! list) with its own `TaskStatus` semantics, and giving it a separate
//! `builtin.tasks` instance means an MCP session can be granted "groom tasks"
//! without also getting `block.rs`'s generic block-editing tools (`block_edit`,
//! `block_splice`, …) — the same reasoning `builtin.shell` /
//! `builtin.shell_readonly` split on. A `BlockKind::Task` block IS an ordinary
//! block underneath (kernel-sequenced, DAG-parented) — this server is just the
//! curated verb set over it, delegating storage to the same `SharedBlockStore`
//! `block.rs` uses.
//!
//! Subtasks reuse the ordinary `parent_id` DAG edge (set at `task_create`
//! time) — no bespoke hierarchy. There is no `task_reparent`/reorder verb:
//! the block store has no cheap "move to a new parent" primitive today
//! (`move_block` only reorders siblings under the same parent), so
//! re-parenting an existing task is deferred (`docs/issues.md`) rather than
//! built as a bespoke mechanism for this slice.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::block_store::SharedBlockStore;
use crate::execution::ExecResult;
use kaijutsu_types::{BlockId, BlockKind, ContentType, Role, Status, TaskStatus};
use kaijutsu_types::{BlockSnapshot, ContextId};

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
use super::adapter::{from_exec_result, to_exec_context};

// ── Typed Params (schemars-derived) ────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCreateParams {
    /// Title/description of the task.
    pub content: String,
    /// Parent task's block ID, for a subtask. Omit for a root task. Subtask
    /// nesting reuses the ordinary DAG `parent_id` edge — no separate
    /// hierarchy field.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Initial status. Defaults to "open" — a freshly created task starting
    /// anywhere else would be an odd thing to represent, but it's accepted
    /// for completeness (e.g. importing an already-in-progress task list).
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskUpdateParams {
    /// Task's block ID.
    pub block_id: String,
    /// Replace the task's title/description. At least one of `content`/
    /// `status` must be given.
    #[serde(default)]
    pub content: Option<String>,
    /// New status ("open" | "in_progress" | "done" | "cancelled").
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCompleteParams {
    /// Task's block ID.
    pub block_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCancelParams {
    /// Task's block ID.
    pub block_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskListParams {
    /// Only list subtasks of this task's block ID. Omit to list root tasks
    /// AND subtasks together (the common "show me everything" case);
    /// combine with `filter` to narrow.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// "open" (Open ∪ InProgress — not yet finished), "done" (Done ∪
    /// Cancelled — finished, either way), or an exact status name ("open",
    /// "in_progress", "done", "cancelled"). Omit for no filter.
    #[serde(default)]
    pub filter: Option<String>,
}

// ── Server ─────────────────────────────────────────────────────────────────

pub struct BuiltinTasksServer {
    instance_id: InstanceId,
    documents: SharedBlockStore,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinTasksServer {
    pub const INSTANCE: &'static str = "builtin.tasks";

    pub fn new(documents: SharedBlockStore) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            documents,
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

/// A validated `task_list` filter — parsed ONCE up front
/// ([`TaskFilter::parse`]), not per-candidate. Parsing per-item (matching
/// only when a task happened to be checked against it) would let an invalid
/// `filter` string silently pass on an empty or non-matching list instead of
/// failing loud — the mistake this two-phase split exists to rule out.
enum TaskFilter {
    All,
    /// Open ∪ InProgress — not yet finished.
    Open,
    /// Done ∪ Cancelled — finished, either way.
    Done,
    Exact(TaskStatus),
}

impl TaskFilter {
    fn parse(filter: Option<&str>) -> McpResult<Self> {
        match filter {
            None => Ok(TaskFilter::All),
            Some("") => Ok(TaskFilter::All),
            Some("open") => Ok(TaskFilter::Open),
            Some("done") => Ok(TaskFilter::Done),
            Some(other) => TaskStatus::from_str(other)
                .map(TaskFilter::Exact)
                .ok_or_else(|| McpError::Protocol(format!("invalid filter: {}", other))),
        }
    }

    fn matches(&self, status: TaskStatus) -> bool {
        match self {
            TaskFilter::All => true,
            TaskFilter::Open => matches!(status, TaskStatus::Open | TaskStatus::InProgress),
            TaskFilter::Done => matches!(status, TaskStatus::Done | TaskStatus::Cancelled),
            TaskFilter::Exact(exact) => status == *exact,
        }
    }
}

fn task_json(snapshot: &BlockSnapshot, version: u64) -> serde_json::Value {
    serde_json::json!({
        "block_id": snapshot.id.to_key(),
        "parent_id": snapshot.parent_id.as_ref().map(|id| id.to_key()),
        "role": snapshot.role.as_str(),
        "status": snapshot.task_status.as_str(),
        "content": snapshot.content,
        "version": version,
    })
}

#[async_trait]
impl McpServerLike for BuiltinTasksServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<TaskCreateParams>(
                &self.instance_id,
                "task_create",
                "Create a task (or subtask, via parent_id) for grooming — household-agent task/plan state.",
            )?,
            tool_def::<TaskUpdateParams>(
                &self.instance_id,
                "task_update",
                "Update a task's content and/or status.",
            )?,
            tool_def::<TaskCompleteParams>(
                &self.instance_id,
                "task_complete",
                "Mark a task done. Shorthand for task_update with status=done.",
            )?,
            tool_def::<TaskCancelParams>(
                &self.instance_id,
                "task_cancel",
                "Mark a task cancelled (deliberately abandoned, distinct from done). Shorthand for task_update with status=cancelled.",
            )?,
            tool_def::<TaskListParams>(
                &self.instance_id,
                "task_list",
                "List tasks in the current context, optionally filtered by parent (subtasks) and/or status bucket (open/done).",
            )?,
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let tool_ctx = to_exec_context(ctx);
        let context_id = tool_ctx.context_id;

        let exec = match params.tool.as_str() {
            "task_create" => {
                let p: TaskCreateParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let status = match &p.status {
                    Some(s) => self.parse_task_status(s)?,
                    None => TaskStatus::default(),
                };
                let parent_id = p
                    .parent_id
                    .as_ref()
                    .map(|s| self.parse_block_id(s))
                    .transpose()?;
                if let Some(ref pid) = parent_id
                    && pid.context_id != context_id
                {
                    return Err(McpError::Protocol(
                        "parent_id must be a task in the current context".into(),
                    ));
                }

                if !self.documents.contains(context_id) {
                    return Err(McpError::Protocol(format!(
                        "no document for context {}",
                        context_id.short()
                    )));
                }

                let block_id = self
                    .documents
                    .insert_block_as(
                        context_id,
                        parent_id.as_ref(),
                        None,
                        Role::Tool,
                        BlockKind::Task,
                        &p.content,
                        Status::Done,
                        ContentType::Plain,
                        Some(tool_ctx.principal_id),
                    )
                    .map_err(|e| McpError::Protocol(e.to_string()))?;

                // A fresh task is Open by construction; only touch task_status
                // when the caller asked for something else.
                if status != TaskStatus::default() {
                    self.documents
                        .set_task_status(context_id, &block_id, status)
                        .map_err(|e| McpError::Protocol(e.to_string()))?;
                }

                let version = self.documents.get(context_id).map(|c| c.version()).unwrap_or(0);
                let res_json = serde_json::json!({
                    "block_id": block_id.to_key(),
                    "status": status.as_str(),
                    "version": version,
                });
                ExecResult::success(res_json.to_string())
            }
            "task_update" => {
                let p: TaskUpdateParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                if p.content.is_none() && p.status.is_none() {
                    return Err(McpError::InvalidParams(serde::de::Error::custom(
                        "task_update requires at least one of content/status",
                    )));
                }
                let (task_context_id, block_id) = self.find_task(&p.block_id)?;

                if let Some(ref content) = p.content {
                    self.replace_content(task_context_id, &block_id, content, &tool_ctx)?;
                }
                if let Some(ref status_str) = p.status {
                    let status = self.parse_task_status(status_str)?;
                    self.documents
                        .set_task_status(task_context_id, &block_id, status)
                        .map_err(|e| McpError::Protocol(e.to_string()))?;
                }

                let version = self
                    .documents
                    .get(task_context_id)
                    .map(|c| c.version())
                    .unwrap_or(0);
                let res_json = serde_json::json!({
                    "block_id": p.block_id,
                    "version": version,
                });
                ExecResult::success(res_json.to_string())
            }
            "task_complete" => {
                let p: TaskCompleteParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (task_context_id, block_id) = self.find_task(&p.block_id)?;
                self.documents
                    .set_task_status(task_context_id, &block_id, TaskStatus::Done)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let version = self
                    .documents
                    .get(task_context_id)
                    .map(|c| c.version())
                    .unwrap_or(0);
                let res_json = serde_json::json!({
                    "block_id": p.block_id,
                    "status": TaskStatus::Done.as_str(),
                    "version": version,
                });
                ExecResult::success(res_json.to_string())
            }
            "task_cancel" => {
                let p: TaskCancelParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let (task_context_id, block_id) = self.find_task(&p.block_id)?;
                self.documents
                    .set_task_status(task_context_id, &block_id, TaskStatus::Cancelled)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let version = self
                    .documents
                    .get(task_context_id)
                    .map(|c| c.version())
                    .unwrap_or(0);
                let res_json = serde_json::json!({
                    "block_id": p.block_id,
                    "status": TaskStatus::Cancelled.as_str(),
                    "version": version,
                });
                ExecResult::success(res_json.to_string())
            }
            "task_list" => {
                let p: TaskListParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let parent_id_filter = p
                    .parent_id
                    .as_ref()
                    .map(|s| self.parse_block_id(s))
                    .transpose()?;
                // Parsed up front — see TaskFilter's doc comment for why a
                // per-item parse would let a bad filter string pass silently
                // on an empty/non-matching list.
                let status_filter = TaskFilter::parse(p.filter.as_deref())?;

                let entry = self
                    .documents
                    .get(context_id)
                    .ok_or_else(|| McpError::Protocol(format!("no document for context {}", context_id.short())))?;
                let version = entry.version();

                let mut tasks = Vec::new();
                for snapshot in entry.doc.blocks_ordered() {
                    if snapshot.kind != BlockKind::Task {
                        continue;
                    }
                    if let Some(ref pid) = parent_id_filter
                        && snapshot.parent_id.as_ref() != Some(pid)
                    {
                        continue;
                    }
                    if !status_filter.matches(snapshot.task_status) {
                        continue;
                    }
                    tasks.push(task_json(&snapshot, version));
                }

                let res_json = serde_json::json!({
                    "tasks": tasks,
                    "count": tasks.len(),
                });
                ExecResult::success(res_json.to_string())
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

impl BuiltinTasksServer {
    fn parse_block_id(&self, s: &str) -> McpResult<BlockId> {
        BlockId::from_key(s)
            .ok_or_else(|| McpError::Protocol(format!("invalid block_id format: {}", s)))
    }

    fn parse_task_status(&self, s: &str) -> McpResult<TaskStatus> {
        TaskStatus::from_str(s).ok_or_else(|| McpError::Protocol(format!("invalid status: {}", s)))
    }

    /// Resolve a task's block ID to `(context_id, block_id)`, verifying the
    /// block exists AND is actually `BlockKind::Task` — fails loud on a
    /// caller passing e.g. a ToolResult block id rather than silently
    /// grooming the wrong kind of block.
    fn find_task(&self, block_id_str: &str) -> McpResult<(ContextId, BlockId)> {
        let block_id = self.parse_block_id(block_id_str)?;
        let context_id = block_id.context_id;

        let entry = self
            .documents
            .get(context_id)
            .ok_or_else(|| McpError::Protocol(format!("no document for context {}", context_id.short())))?;
        let snapshot = entry
            .doc
            .get_block_snapshot(&block_id)
            .ok_or_else(|| McpError::Protocol(format!("block not found: {}", block_id_str)))?;
        if snapshot.kind != BlockKind::Task {
            return Err(McpError::Protocol(format!(
                "block {} is a {}, not a task",
                block_id_str,
                snapshot.kind.as_str()
            )));
        }
        Ok((context_id, block_id))
    }

    /// Full-replace a task's content (title/description) — deletes the
    /// current text and inserts the new text in one edit.
    fn replace_content(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        new_content: &str,
        tool_ctx: &crate::execution::ExecContext,
    ) -> McpResult<()> {
        let old_char_count = {
            let entry = self
                .documents
                .get(context_id)
                .ok_or_else(|| McpError::Protocol("document not found".into()))?;
            entry
                .doc
                .get_block_snapshot(block_id)
                .ok_or_else(|| McpError::Protocol(format!("block not found: {}", block_id.to_key())))?
                .content
                .chars()
                .count()
        };
        self.documents
            .edit_text_as(
                context_id,
                block_id,
                0,
                new_content,
                old_char_count,
                Some(tool_ctx.principal_id),
            )
            .map_err(|e| McpError::Protocol(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::block_store::{DocumentKind, shared_block_store_with_db};
    use crate::kernel_db::{DocumentRow, KernelDb};
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use kaijutsu_types::{PrincipalId, now_millis};

    async fn setup() -> (Arc<Broker>, CallContext, SharedBlockStore) {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let creator = PrincipalId::new();
        let ws_id = {
            let g = db.lock();
            g.get_or_create_default_workspace(creator).unwrap()
        };
        let store = shared_block_store_with_db(db.clone(), ws_id, creator);

        let mut ctx = CallContext::test();
        ctx.principal_id = creator;
        {
            let g = db.lock();
            g.insert_document(&DocumentRow {
                document_id: ctx.context_id,
                workspace_id: ws_id,
                doc_kind: DocumentKind::File,
                language: None,
                path: None,
                created_at: now_millis() as i64,
                created_by: creator,
            })
            .unwrap();
        }
        store
            .create_document(ctx.context_id, DocumentKind::File, None)
            .unwrap();

        let server = Arc::new(BuiltinTasksServer::new(store.clone()));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        (broker, ctx, store)
    }

    async fn call(broker: &Broker, ctx: &CallContext, tool: &str, args: serde_json::Value) -> KernelToolResult {
        call_res(broker, ctx, tool, args).await.unwrap()
    }

    async fn call_res(
        broker: &Broker,
        ctx: &CallContext,
        tool: &str,
        args: serde_json::Value,
    ) -> McpResult<KernelToolResult> {
        broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(BuiltinTasksServer::INSTANCE),
                    tool: tool.to_string(),
                    arguments: args,
                },
                ctx,
                CancellationToken::new(),
            )
            .await
    }

    fn text_of(r: &KernelToolResult) -> String {
        match r.content.first() {
            Some(ToolContent::Text(s)) => s.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn json_of(r: &KernelToolResult) -> serde_json::Value {
        serde_json::from_str(&text_of(r)).unwrap()
    }

    // ── list_tools ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_tools_exposes_all_five() {
        let (broker, ctx, _store) = setup().await;
        let visible = {
            let mut binding = crate::mcp::ContextToolBinding::new();
            binding.allow(InstanceId::new(BuiltinTasksServer::INSTANCE));
            broker.set_binding(ctx.context_id, binding).await;
            broker.list_visible_tools(ctx.context_id, &ctx).await.unwrap()
        };
        let names: Vec<_> = visible.iter().map(|(n, _)| n.as_str()).collect();
        for expected in [
            "task_create",
            "task_update",
            "task_complete",
            "task_cancel",
            "task_list",
        ] {
            assert!(names.contains(&expected), "missing {}", expected);
        }
    }

    // ── task_create ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_create_via_broker_defaults_to_open() {
        let (broker, ctx, store) = setup().await;
        let result = call(
            &broker,
            &ctx,
            "task_create",
            serde_json::json!({ "content": "Buy milk" }),
        )
        .await;
        assert!(!result.is_error, "unexpected error: {:?}", result.content);
        let json = json_of(&result);
        assert_eq!(json["status"], "open");

        let block_id = BlockId::from_key(json["block_id"].as_str().unwrap()).unwrap();
        let snap = store
            .get_block_snapshot(ctx.context_id, &block_id)
            .unwrap()
            .unwrap();
        assert_eq!(snap.kind, BlockKind::Task);
        assert_eq!(snap.task_status, TaskStatus::Open);
        assert_eq!(snap.content, "Buy milk");
        // Role follows content-authorship convention for tool-created rich
        // content (svg_block/abc_block precedent) — NOT forced to System the
        // way Notification/Resource are.
        assert_eq!(snap.role, Role::Tool);
    }

    #[tokio::test]
    async fn task_create_with_explicit_status() {
        let (broker, ctx, _store) = setup().await;
        let result = call(
            &broker,
            &ctx,
            "task_create",
            serde_json::json!({ "content": "Already going", "status": "in_progress" }),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(json_of(&result)["status"], "in_progress");
    }

    #[tokio::test]
    async fn task_create_subtask_sets_parent_id() {
        let (broker, ctx, store) = setup().await;
        let parent = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Grocery run" }),
            )
            .await,
        );
        let parent_id = parent["block_id"].as_str().unwrap().to_string();

        let child = call(
            &broker,
            &ctx,
            "task_create",
            serde_json::json!({ "content": "Buy oat milk", "parent_id": parent_id }),
        )
        .await;
        assert!(!child.is_error);
        let child_json = json_of(&child);
        let child_id = BlockId::from_key(child_json["block_id"].as_str().unwrap()).unwrap();
        let snap = store
            .get_block_snapshot(ctx.context_id, &child_id)
            .unwrap()
            .unwrap();
        assert_eq!(snap.parent_id, BlockId::from_key(&parent_id));
    }

    #[tokio::test]
    async fn task_create_rejects_invalid_status() {
        let (broker, ctx, _store) = setup().await;
        let res = call_res(
            &broker,
            &ctx,
            "task_create",
            serde_json::json!({ "content": "x", "status": "bogus" }),
        )
        .await;
        assert!(res.is_err(), "expected invalid status to be rejected");
    }

    // ── task_update ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_update_status_and_content() {
        let (broker, ctx, store) = setup().await;
        let created = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Buy milk" }),
            )
            .await,
        );
        let block_id_str = created["block_id"].as_str().unwrap().to_string();
        let block_id = BlockId::from_key(&block_id_str).unwrap();

        let update = call(
            &broker,
            &ctx,
            "task_update",
            serde_json::json!({
                "block_id": block_id_str,
                "content": "Buy oat milk",
                "status": "in_progress",
            }),
        )
        .await;
        assert!(!update.is_error, "update failed: {}", text_of(&update));

        let snap = store
            .get_block_snapshot(ctx.context_id, &block_id)
            .unwrap()
            .unwrap();
        assert_eq!(snap.content, "Buy oat milk");
        assert_eq!(snap.task_status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn task_update_requires_content_or_status() {
        let (broker, ctx, _store) = setup().await;
        let created = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Buy milk" }),
            )
            .await,
        );
        let res = call_res(
            &broker,
            &ctx,
            "task_update",
            serde_json::json!({ "block_id": created["block_id"] }),
        )
        .await;
        assert!(res.is_err(), "task_update with neither field must fail loud");
    }

    #[tokio::test]
    async fn task_update_rejects_non_task_block() {
        let (broker, ctx, store) = setup().await;
        // A plain text block, not a task.
        let text_id = store
            .insert_block(
                ctx.context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "just text",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let res = call_res(
            &broker,
            &ctx,
            "task_update",
            serde_json::json!({ "block_id": text_id.to_key(), "status": "done" }),
        )
        .await;
        assert!(res.is_err(), "must reject grooming a non-task block");
    }

    // ── task_complete / task_cancel ─────────────────────────────────────

    #[tokio::test]
    async fn task_complete_sets_done() {
        let (broker, ctx, store) = setup().await;
        let created = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Buy milk" }),
            )
            .await,
        );
        let block_id_str = created["block_id"].as_str().unwrap().to_string();
        let result = call(
            &broker,
            &ctx,
            "task_complete",
            serde_json::json!({ "block_id": block_id_str }),
        )
        .await;
        assert!(!result.is_error);
        let block_id = BlockId::from_key(&block_id_str).unwrap();
        let snap = store
            .get_block_snapshot(ctx.context_id, &block_id)
            .unwrap()
            .unwrap();
        assert_eq!(snap.task_status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn task_cancel_sets_cancelled() {
        let (broker, ctx, store) = setup().await;
        let created = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Buy milk" }),
            )
            .await,
        );
        let block_id_str = created["block_id"].as_str().unwrap().to_string();
        let result = call(
            &broker,
            &ctx,
            "task_cancel",
            serde_json::json!({ "block_id": block_id_str }),
        )
        .await;
        assert!(!result.is_error);
        let block_id = BlockId::from_key(&block_id_str).unwrap();
        let snap = store
            .get_block_snapshot(ctx.context_id, &block_id)
            .unwrap()
            .unwrap();
        assert_eq!(snap.task_status, TaskStatus::Cancelled);
    }

    // ── task_list ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_list_filters_open_and_done() {
        let (broker, ctx, _store) = setup().await;
        let open = json_of(
            &call(&broker, &ctx, "task_create", serde_json::json!({ "content": "Open one" })).await,
        );
        let in_progress_id = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "In progress one" }),
            )
            .await,
        )["block_id"]
            .as_str()
            .unwrap()
            .to_string();
        call(
            &broker,
            &ctx,
            "task_update",
            serde_json::json!({ "block_id": in_progress_id, "status": "in_progress" }),
        )
        .await;
        let done_id = json_of(
            &call(&broker, &ctx, "task_create", serde_json::json!({ "content": "Done one" })).await,
        )["block_id"]
            .as_str()
            .unwrap()
            .to_string();
        call(&broker, &ctx, "task_complete", serde_json::json!({ "block_id": done_id })).await;
        let cancelled_id = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Cancelled one" }),
            )
            .await,
        )["block_id"]
            .as_str()
            .unwrap()
            .to_string();
        call(&broker, &ctx, "task_cancel", serde_json::json!({ "block_id": cancelled_id })).await;

        // Unfiltered: all four.
        let all = json_of(&call(&broker, &ctx, "task_list", serde_json::json!({})).await);
        assert_eq!(all["count"], 4);

        // "open" bucket: Open ∪ InProgress = 2.
        let open_bucket = json_of(
            &call(&broker, &ctx, "task_list", serde_json::json!({ "filter": "open" })).await,
        );
        assert_eq!(open_bucket["count"], 2);

        // "done" bucket: Done ∪ Cancelled = 2.
        let done_bucket = json_of(
            &call(&broker, &ctx, "task_list", serde_json::json!({ "filter": "done" })).await,
        );
        assert_eq!(done_bucket["count"], 2);

        // Exact status filter.
        let exact = json_of(
            &call(
                &broker,
                &ctx,
                "task_list",
                serde_json::json!({ "filter": "cancelled" }),
            )
            .await,
        );
        assert_eq!(exact["count"], 1);

        let _ = open; // silence unused warning if content unused beyond creation
    }

    #[tokio::test]
    async fn task_list_filters_by_parent_for_subtasks() {
        let (broker, ctx, _store) = setup().await;
        let parent_id = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Grocery run" }),
            )
            .await,
        )["block_id"]
            .as_str()
            .unwrap()
            .to_string();
        call(
            &broker,
            &ctx,
            "task_create",
            serde_json::json!({ "content": "Buy oat milk", "parent_id": parent_id }),
        )
        .await;
        call(
            &broker,
            &ctx,
            "task_create",
            serde_json::json!({ "content": "Unrelated root task" }),
        )
        .await;

        let subtasks = json_of(
            &call(
                &broker,
                &ctx,
                "task_list",
                serde_json::json!({ "parent_id": parent_id }),
            )
            .await,
        );
        assert_eq!(subtasks["count"], 1);
        assert_eq!(subtasks["tasks"][0]["content"], "Buy oat milk");
    }

    /// No tasks exist yet in this context (setup() creates none) — a
    /// per-candidate filter check would never run and this would pass
    /// silently. Caught exactly that bug during development (`TaskFilter`
    /// is now parsed once up front, not per item); kept as a deliberate
    /// empty-list regression pin, not swapped for a populated one.
    #[tokio::test]
    async fn task_list_rejects_invalid_filter() {
        let (broker, ctx, _store) = setup().await;
        let res = call_res(
            &broker,
            &ctx,
            "task_list",
            serde_json::json!({ "filter": "bogus" }),
        )
        .await;
        assert!(res.is_err());
    }

    // ── Cross-client visibility ────────────────────────────────────────
    //
    // Full SSH/two-broker e2e is out of reach for a unit test, but the
    // thing that actually makes task state "kernel-owned multi-frontend
    // sync for free" is that grooming rides the SAME `BlockFlow` bus every
    // other block mutation does — so a second subscriber (an app view, a
    // sibling context) sees a task's creation and status changes exactly
    // like it would for any other block, no task-specific plumbing. That
    // claim IS cheaply testable: attach a `SharedBlockFlowBus` to the
    // store and subscribe before grooming.

    #[tokio::test]
    async fn task_groom_is_visible_on_the_block_flow_bus() {
        use crate::flows::{BlockFlow, shared_block_flow_bus};

        let principal = PrincipalId::new();
        let bus = shared_block_flow_bus(64);
        let store: SharedBlockStore =
            Arc::new(crate::block_store::BlockStore::with_flows(principal, bus.clone()));

        let mut ctx = CallContext::test();
        ctx.principal_id = principal;
        store
            .create_document(ctx.context_id, DocumentKind::File, None)
            .unwrap();

        let server = Arc::new(BuiltinTasksServer::new(store.clone()));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        // A second "client" subscribes to the same context's block flows —
        // this is the multi-frontend observer (app view / sibling context),
        // wired up BEFORE any grooming happens, same as a real attach.
        let mut subscriber = bus.subscribe("block.*");

        let created = json_of(
            &call(
                &broker,
                &ctx,
                "task_create",
                serde_json::json!({ "content": "Buy milk" }),
            )
            .await,
        );
        let block_id_str = created["block_id"].as_str().unwrap().to_string();
        call(
            &broker,
            &ctx,
            "task_complete",
            serde_json::json!({ "block_id": block_id_str }),
        )
        .await;

        // The subscriber must observe BOTH the creation (Inserted, kind =
        // Task, task_status = Open) and the completion (MetadataChanged,
        // task_status = Done) — without task_create/task_complete having
        // to know or care that a second reader exists.
        let mut saw_inserted_task = false;
        let mut saw_done_metadata = false;
        while let Some(msg) = subscriber.try_recv() {
            match msg.payload {
                BlockFlow::Inserted { block, .. } if block.kind == BlockKind::Task => {
                    assert_eq!(block.task_status, TaskStatus::Open);
                    saw_inserted_task = true;
                }
                BlockFlow::MetadataChanged { metadata, .. }
                    if metadata.task_status == TaskStatus::Done =>
                {
                    saw_done_metadata = true;
                }
                _ => {}
            }
        }
        assert!(saw_inserted_task, "second subscriber must see the task's creation");
        assert!(
            saw_done_metadata,
            "second subscriber must see the task's completion, live, via the same \
             MetadataChanged channel every other block mutation already uses"
        );
    }
}
