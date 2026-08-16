//! `BackgroundServer` — companion tools for the `shell` tool's
//! `background: true` jobs (`background_exec.rs`).
//!
//! Sibling of `builtin.shell`/`builtin.shell_readonly`, riding the SAME
//! `facade:shell` bit (see `mcp::binding::FACADE_PROJECTED_INSTANCES`) rather
//! than a new capability axis: these tools are only meaningful to a context
//! that already has a shell, and whoever can shell out should be able to
//! manage what they backgrounded, with no separate rc grant to remember.
//! `kill_background_process` additionally re-checks the `exec` authority
//! (mirroring `shell.rs`'s `background: true` gate) since it's a mutating
//! action on a host process — the same authority that started it is required
//! to stop it, not a weaker one.
//!
//! `list_background_processes`/`read_background_output` are scoped to the
//! calling context (`BackgroundRegistry::list_for_context`/
//! `get_for_context`) — a context never sees, reads, or kills another
//! context's background process, existence included (no leak through a
//! "not found" vs "not yours" distinction).

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
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult, ToolContent};

use crate::background_exec::BackgroundId;

/// Most bytes one `read_background_output` call returns. Deliberately well
/// under the broker's default 64 KiB `max_result_bytes` (`mcp/policy.rs:43`)
/// so a read is never subject to the broker's middle-truncation — see the
/// `next_offset` derivation in `call_tool` for why that would otherwise make
/// polling lossy. A caller drains a large buffer by polling with the returned
/// `next_offset` until `has_more` is false.
const MAX_READ_BYTES: usize = 32 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListBackgroundParams {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadBackgroundOutputParams {
    /// The `background_id` a `shell` call with `background: true` returned.
    pub id: String,
    /// Byte offset into the accumulated output to read from. Omit (or 0) to
    /// read from the start; pass back the previous response's `next_offset`
    /// to fetch only what's new since your last poll.
    #[serde(default)]
    pub offset: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KillBackgroundParams {
    /// The `background_id` a `shell` call with `background: true` returned.
    pub id: String,
}

fn tool_def<P: JsonSchema>(instance: &InstanceId, name: &str, description: &str) -> McpResult<KernelTool> {
    let schema = schemars::schema_for!(P);
    Ok(KernelTool {
        instance: instance.clone(),
        name: name.to_string(),
        description: Some(description.to_string()),
        input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
    })
}

/// In-kernel broker server backing `list_background_processes` /
/// `read_background_output` / `kill_background_process`. Holds `Weak<Broker>`
/// (the broker owns this instance's `Arc`) to reach the shared `KjDispatcher`
/// → `Kernel::background_processes()`, mirroring `ShellServer`'s own
/// `Weak<Broker>` pattern.
pub struct BackgroundServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BackgroundServer {
    pub const INSTANCE: &'static str = "builtin.background";
    pub const TOOL_LIST: &'static str = "list_background_processes";
    pub const TOOL_READ: &'static str = "read_background_output";
    pub const TOOL_KILL: &'static str = "kill_background_process";

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

    fn parse_id(raw: &str) -> McpResult<BackgroundId> {
        BackgroundId::parse(raw)
            .ok_or_else(|| McpError::Protocol(format!("invalid background process id: {raw}")))
    }
}

#[async_trait]
impl McpServerLike for BackgroundServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![
            tool_def::<ListBackgroundParams>(
                &self.instance_id,
                Self::TOOL_LIST,
                "List background processes started with `shell(background: true)` in this context — id, pid, command, status, exit code (if finished), and the block their output streams into.",
            )?,
            tool_def::<ReadBackgroundOutputParams>(
                &self.instance_id,
                Self::TOOL_READ,
                "Read accumulated output from a background process, optionally from a byte offset (pass back the previous `next_offset` to poll only what's new).",
            )?,
            tool_def::<KillBackgroundParams>(
                &self.instance_id,
                Self::TOOL_KILL,
                "Stop a running background process (SIGKILL to its process group). A no-op if it already exited.",
            )?,
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let broker = self.broker()?;
        let dispatcher = broker
            .kj_dispatcher()
            .await
            .ok_or_else(|| McpError::InstanceDown {
                instance: self.instance_id.clone(),
                reason: "kj dispatcher not wired (Broker::set_kj_dispatcher)".to_string(),
            })?;
        let registry = dispatcher.kernel().background_processes();

        match params.tool.as_str() {
            name if name == Self::TOOL_LIST => {
                let _p: ListBackgroundParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let list = registry.list_for_context(ctx.context_id);
                let body = if list.is_empty() {
                    "no background processes in this context".to_string()
                } else {
                    list.iter()
                        .map(|e| {
                            format!(
                                "{} pid={} [{}{}] {}",
                                e.id,
                                e.pid,
                                e.status,
                                e.exit_code.map(|c| format!("={c}")).unwrap_or_default(),
                                e.command
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Text(body)],
                    structured: Some(
                        serde_json::to_value(&list).map_err(McpError::InvalidParams)?,
                    ),
                })
            }
            name if name == Self::TOOL_READ => {
                let p: ReadBackgroundOutputParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let id = Self::parse_id(&p.id)?;
                let entry = registry.get_for_context(id, ctx.context_id).ok_or_else(|| {
                    McpError::Protocol(format!("no background process {} in this context", p.id))
                })?;
                let block_id = kaijutsu_types::BlockId::from_key(&entry.block_id).ok_or_else(|| {
                    McpError::Protocol(format!(
                        "background process {} has a malformed block id: {}",
                        p.id, entry.block_id
                    ))
                })?;
                let blocks = dispatcher.block_store();
                let snap = blocks
                    .get_block_snapshot(ctx.context_id, &block_id)
                    .map_err(|e| McpError::Protocol(format!("read background output: {e}")))?
                    .ok_or_else(|| {
                        McpError::Protocol("background output block no longer exists".to_string())
                    })?;
                let content = snap.content;
                let mut start = (p.offset.unwrap_or(0) as usize).min(content.len());
                while start > 0 && !content.is_char_boundary(start) {
                    start -= 1;
                }
                // Bound what one read returns, and derive `next_offset` from
                // what we ACTUALLY returned rather than from the total length.
                //
                // Why this matters: the broker enforces `max_result_bytes`
                // (64 KiB by default, `mcp/policy.rs:43`) on every tool result
                // (`broker.rs:1392`). An oversized read doesn't just lose its
                // middle — `truncate_result_to_budget` drops `structured`
                // UNCONDITIONALLY (`broker.rs:2788`, "an indivisible blob"),
                // so the caller loses `next_offset`/`has_more` entirely and
                // has no way to resume the poll at all. Staying under the
                // budget keeps the envelope intact and makes `next_offset` a
                // truthful "you have everything up to here" promise, so
                // draining a large buffer is lossless by construction.
                let end = {
                    let mut e = (start + MAX_READ_BYTES).min(content.len());
                    while e > start && !content.is_char_boundary(e) {
                        e -= 1;
                    }
                    e
                };
                let slice = &content[start..end];
                let next_offset = end as u64;
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Text(slice.to_string())],
                    structured: Some(serde_json::json!({
                        "next_offset": next_offset,
                        "total_bytes": content.len(),
                        "has_more": end < content.len(),
                        "status": entry.status,
                        "exit_code": entry.exit_code,
                        "pid": entry.pid,
                    })),
                })
            }
            name if name == Self::TOOL_KILL => {
                let p: KillBackgroundParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                // Killing is a mutating host action — require the SAME `exec`
                // authority starting a background process required (matches
                // the "same authority, not a weaker one" rule in
                // `background_exec.rs`'s module docs).
                let exec_granted = broker
                    .binding(&ctx.context_id)
                    .await
                    .is_some_and(|b| b.allows(&crate::mcp::Capability::Exec));
                if !exec_granted {
                    return Err(McpError::Protocol(
                        "kill_background_process requires the `exec` authority (deny-by-default — see `kj binding allow exec`)"
                            .to_string(),
                    ));
                }
                let id = Self::parse_id(&p.id)?;
                let entry = registry.get_for_context(id, ctx.context_id).ok_or_else(|| {
                    McpError::Protocol(format!("no background process {} in this context", p.id))
                })?;
                if entry.status != "running" {
                    return Ok(KernelToolResult {
                        is_error: false,
                        content: vec![ToolContent::Text(format!(
                            "background process {} already {}",
                            p.id, entry.status
                        ))],
                        structured: Some(serde_json::json!({"status": entry.status, "exit_code": entry.exit_code})),
                    });
                }
                // The entry could have finished in the gap between the
                // `get_for_context` snapshot above and this call (TOCTOU —
                // the process is still async-supervised) — `cancel` reports
                // that honestly via its return value rather than the
                // response always claiming a signal was sent.
                let signalled = registry.cancel(id, ctx.context_id);
                if !signalled {
                    return Ok(KernelToolResult {
                        is_error: false,
                        content: vec![ToolContent::Text(format!(
                            "background process {} finished before the kill reached it",
                            p.id
                        ))],
                        structured: Some(serde_json::json!({"status": "raced-to-completion"})),
                    });
                }
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Text(format!(
                        "sent kill to background process {}",
                        p.id
                    ))],
                    structured: Some(serde_json::json!({"status": "killing"})),
                })
            }
            other => Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: other.to_string(),
            }),
        }
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_exec::SpawnBackgroundParams;
    use crate::kj::test_helpers::{register_context, test_dispatcher};
    use crate::mcp::binding::{Capability, ContextToolBinding};
    use crate::mcp::servers::ShellServer;
    use crate::mcp::{InstancePolicy, KernelCallParams};
    use kaijutsu_types::{BlockKind, ContentType, Role, Status};
    use kaijutsu_types::{DocKind, PrincipalId, SessionId};

    async fn wired() -> (Arc<Broker>, Arc<crate::kj::KjDispatcher>) {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let broker = Arc::new(Broker::new());
        broker.set_kj_dispatcher(&d).await;
        broker
            .register(
                Arc::new(ShellServer::new(Arc::downgrade(&broker))),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register(
                Arc::new(BackgroundServer::new(Arc::downgrade(&broker))),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        (broker, d)
    }

    fn call(tool: &str, args: serde_json::Value) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(BackgroundServer::INSTANCE),
            tool: tool.to_string(),
            arguments: args,
        }
    }

    async fn wait_for_status(
        broker: &Broker,
        cc: &CallContext,
        id: &str,
        want: &str,
        timeout: std::time::Duration,
    ) -> serde_json::Value {
        let start = std::time::Instant::now();
        loop {
            let r = broker
                .call_tool(
                    call(BackgroundServer::TOOL_LIST, serde_json::json!({})),
                    cc,
                    CancellationToken::new(),
                )
                .await
                .expect("list should succeed");
            let structured = r.structured.expect("structured envelope");
            if let Some(entry) = structured.as_array().and_then(|a| {
                a.iter().find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
            }) && entry.get("status").and_then(|v| v.as_str()) == Some(want)
            {
                return entry.clone();
            }
            if start.elapsed() > timeout {
                panic!("timed out waiting for background process {id} to reach status {want}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Polling a buffer larger than one read must be LOSSLESS: each response
    /// is bounded by `MAX_READ_BYTES`, `next_offset` reflects what was
    /// actually returned (never the total), `has_more` says whether to keep
    /// going, and concatenating every slice reproduces the full output byte
    /// for byte with no gap and no overlap.
    ///
    /// Without the bound this regresses in a way that is easy to miss: the
    /// tool would hand back the whole buffer with `next_offset =
    /// content.len()`, the broker's `max_result_bytes` (64 KiB,
    /// `mcp/policy.rs:43`) would truncate it from the middle
    /// (`broker.rs:1392`), and the caller — having been told it now holds
    /// everything up to `next_offset` — could never retrieve the removed
    /// middle. The assertion that the reassembled text equals the block's
    /// real content is the one that fails.
    #[tokio::test]
    async fn read_background_output_polls_losslessly_past_one_read_bound() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-poll"), None, principal);
        d.block_store()
            .create_document(ctx_id, DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        // Comfortably more than one MAX_READ_BYTES slice, so at least three
        // polls are needed to drain it.
        let total = MAX_READ_BYTES * 2 + 1024;
        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({
                        "command": format!("yes kaijutsu | head -c {total}"),
                        "background": true,
                    }),
                },
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("background start should succeed");
        let bg_id = start.structured.unwrap()["background_id"]
            .as_str()
            .unwrap()
            .to_string();

        wait_for_status(&broker, &cc, &bg_id, "exited", std::time::Duration::from_secs(15)).await;

        let mut offset = 0u64;
        let mut assembled = String::new();
        let mut polls = 0;
        loop {
            let read = broker
                .call_tool(
                    call(
                        BackgroundServer::TOOL_READ,
                        serde_json::json!({"id": bg_id, "offset": offset}),
                    ),
                    &cc,
                    CancellationToken::new(),
                )
                .await
                .expect("read should succeed");
            let chunk = match read.content.first().unwrap() {
                ToolContent::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            let structured = read.structured.expect("structured envelope");

            assert!(
                chunk.len() <= MAX_READ_BYTES,
                "a single read must stay within MAX_READ_BYTES so the broker never \
                 middle-truncates it, got {} bytes",
                chunk.len()
            );
            let next = structured["next_offset"].as_u64().expect("next_offset");
            assert_eq!(
                next,
                offset + chunk.len() as u64,
                "next_offset must advance by exactly what was returned — reporting the \
                 total length instead is what makes polling skip content"
            );

            assembled.push_str(&chunk);
            offset = next;
            polls += 1;
            assert!(polls < 100, "polling failed to terminate");

            if !structured["has_more"].as_bool().expect("has_more") {
                assert_eq!(
                    next,
                    structured["total_bytes"].as_u64().expect("total_bytes"),
                    "has_more=false must mean the caller really has reached the end"
                );
                break;
            }
        }

        assert!(polls >= 3, "expected several bounded polls, took {polls}");

        // The reassembled stream must equal the block's real content exactly.
        let entry = d
            .kernel()
            .background_processes()
            .get_for_context(BackgroundId::parse(&bg_id).unwrap(), ctx_id)
            .expect("entry");
        let block_id = kaijutsu_types::BlockId::from_key(&entry.block_id).unwrap();
        let real = d
            .block_store()
            .get_block_snapshot(ctx_id, &block_id)
            .unwrap()
            .unwrap()
            .content;
        assert_eq!(
            assembled, real,
            "polling must reproduce the output exactly — no gap, no overlap"
        );
    }

    /// End-to-end: `shell(background: true)` starts a real host process, and
    /// `list_background_processes`/`read_background_output` (the sibling
    /// server, gated by the SAME `facade:shell` bit — no separate grant)
    /// observe it through to completion.
    #[tokio::test]
    async fn shell_background_true_is_observable_via_the_sibling_server() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg"), None, principal);
        d.block_store()
            .create_document(ctx_id, DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({"command": "echo hi-bg", "background": true}),
                },
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("background start should succeed");
        assert!(!start.is_error);
        let bg_id = start.structured.unwrap()["background_id"]
            .as_str()
            .unwrap()
            .to_string();

        let entry = wait_for_status(&broker, &cc, &bg_id, "exited", std::time::Duration::from_secs(5)).await;
        assert_eq!(entry["exit_code"], serde_json::json!(0));

        let read = broker
            .call_tool(
                call(BackgroundServer::TOOL_READ, serde_json::json!({"id": bg_id})),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("read should succeed");
        match read.content.first().unwrap() {
            ToolContent::Text(s) => assert!(s.contains("hi-bg"), "got: {s:?}"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(read.structured.unwrap()["exit_code"], serde_json::json!(0));
    }

    /// `read_background_output`'s `offset` must let a poller fetch only what
    /// arrived since the last read, not the whole block again.
    #[tokio::test]
    async fn read_background_output_offset_returns_only_new_bytes() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bgoff"), None, principal);
        d.block_store()
            .create_document(ctx_id, DocKind::Conversation, None)
            .unwrap();
        let block_id = d
            .block_store()
            .insert_block_as(
                ctx_id,
                None,
                None,
                Role::Tool,
                BlockKind::ToolResult,
                String::new(),
                Status::Running,
                ContentType::Plain,
                Some(principal),
            )
            .unwrap();
        let registry = d.kernel().background_processes();
        let bg_id = crate::background_exec::spawn_background(
            registry,
            d.block_store(),
            SpawnBackgroundParams {
                command: "printf 'AAAA'; sleep 0.3; printf 'BBBB'".to_string(),
                cwd: std::env::temp_dir(),
                env: vec![(
                    "PATH".to_string(),
                    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
                )],
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        broker.set_binding(ctx_id, binding).await;

        // First read: wait for "AAAA" to land, then read from the start.
        let start = std::time::Instant::now();
        let first = loop {
            let r = broker
                .call_tool(
                    call(BackgroundServer::TOOL_READ, serde_json::json!({"id": bg_id.to_string()})),
                    &cc,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let text = match r.content.first().unwrap() {
                ToolContent::Text(s) => s.clone(),
                _ => unreachable!(),
            };
            if text.contains("AAAA") {
                break r;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for first chunk");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let next_offset = first.structured.unwrap()["next_offset"].as_u64().unwrap();

        // Second read from next_offset, after BBBB has landed, must contain
        // BBBB but NOT re-send AAAA.
        wait_for_status(&broker, &cc, &bg_id.to_string(), "exited", std::time::Duration::from_secs(5)).await;
        let second = broker
            .call_tool(
                call(
                    BackgroundServer::TOOL_READ,
                    serde_json::json!({"id": bg_id.to_string(), "offset": next_offset}),
                ),
                &cc,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match second.content.first().unwrap() {
            ToolContent::Text(s) => {
                assert!(s.contains("BBBB"), "got: {s:?}");
                assert!(!s.contains("AAAA"), "offset read must not repeat earlier bytes, got: {s:?}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// `kill_background_process` must actually stop a running process and
    /// report it via `list`/`read` afterward.
    #[tokio::test]
    async fn kill_background_process_stops_a_running_job() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bgkill"), None, principal);
        d.block_store()
            .create_document(ctx_id, DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({"command": "sleep 30", "background": true}),
                },
                &cc,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let bg_id = start.structured.unwrap()["background_id"].as_str().unwrap().to_string();

        wait_for_status(&broker, &cc, &bg_id, "running", std::time::Duration::from_secs(2)).await;

        let killed = broker
            .call_tool(
                call(BackgroundServer::TOOL_KILL, serde_json::json!({"id": bg_id})),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("kill should succeed");
        assert!(!killed.is_error);

        wait_for_status(&broker, &cc, &bg_id, "killed", std::time::Duration::from_secs(5)).await;
    }

    /// One context must never see, read, or kill another context's
    /// background process — not even to learn it exists.
    #[tokio::test]
    async fn cross_context_background_process_is_invisible() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let owner_ctx = register_context(&d, Some("owner"), None, principal);
        let other_ctx = register_context(&d, Some("other"), None, principal);
        d.block_store().create_document(owner_ctx, DocKind::Conversation, None).unwrap();
        d.block_store().create_document(other_ctx, DocKind::Conversation, None).unwrap();

        let mut owner_binding = ContextToolBinding::new();
        owner_binding.grant(Capability::Facade("shell".into()));
        owner_binding.grant(Capability::Exec);
        broker.set_binding(owner_ctx, owner_binding).await;
        let mut other_binding = ContextToolBinding::new();
        other_binding.grant(Capability::Facade("shell".into()));
        other_binding.grant(Capability::Exec);
        broker.set_binding(other_ctx, other_binding).await;

        let owner_cc = CallContext::new(principal, owner_ctx, SessionId::new(), d.kernel_id());
        let other_cc = CallContext::new(principal, other_ctx, SessionId::new(), d.kernel_id());

        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({"command": "sleep 30", "background": true}),
                },
                &owner_cc,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let bg_id = start.structured.unwrap()["background_id"].as_str().unwrap().to_string();

        // `other_ctx` must not see it in its own list.
        let other_list = broker
            .call_tool(call(BackgroundServer::TOOL_LIST, serde_json::json!({})), &other_cc, CancellationToken::new())
            .await
            .unwrap();
        let ids: Vec<String> = other_list
            .structured
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        assert!(!ids.contains(&bg_id), "other context must not see owner's background process");

        // Nor read it.
        let read_err = broker
            .call_tool(
                call(BackgroundServer::TOOL_READ, serde_json::json!({"id": bg_id.clone()})),
                &other_cc,
                CancellationToken::new(),
            )
            .await;
        assert!(read_err.is_err(), "reading another context's process must fail");

        // Nor kill it.
        let kill_err = broker
            .call_tool(
                call(BackgroundServer::TOOL_KILL, serde_json::json!({"id": bg_id.clone()})),
                &other_cc,
                CancellationToken::new(),
            )
            .await;
        assert!(kill_err.is_err(), "killing another context's process must fail");

        // Clean up: owner kills its own.
        broker
            .call_tool(call(BackgroundServer::TOOL_KILL, serde_json::json!({"id": bg_id})), &owner_cc, CancellationToken::new())
            .await
            .unwrap();
    }

    /// Deny-by-default: without `facade:shell`, none of the three tools are
    /// even visible (mirrors `shell.rs`'s `no_facade_is_denied_at_the_gate`).
    #[tokio::test]
    async fn no_facade_hides_all_three_tools() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("nofacade"), None, principal);
        broker.set_binding(ctx_id, ContextToolBinding::new()).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        for tool in [
            BackgroundServer::TOOL_LIST,
            BackgroundServer::TOOL_READ,
            BackgroundServer::TOOL_KILL,
        ] {
            assert!(
                !visible.iter().any(|(name, _)| name == tool),
                "{tool} must not be visible without facade:shell: {visible:?}"
            );
        }

        let err = broker
            .call_tool(call(BackgroundServer::TOOL_LIST, serde_json::json!({})), &cc, CancellationToken::new())
            .await;
        assert!(err.is_err(), "list_background_processes must be denied at the gate without facade:shell");
    }

    /// `kill_background_process` needs `exec` on top of `facade:shell` — a
    /// context that can list/read (facade:shell alone) but never granted
    /// `exec` must not be able to kill.
    #[tokio::test]
    async fn kill_requires_exec_authority_even_with_facade_shell() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let owner_ctx = register_context(&d, Some("killowner"), None, principal);
        d.block_store().create_document(owner_ctx, DocKind::Conversation, None).unwrap();
        let mut owner_binding = ContextToolBinding::new();
        owner_binding.grant(Capability::Facade("shell".into()));
        owner_binding.grant(Capability::Exec);
        broker.set_binding(owner_ctx, owner_binding).await;
        let owner_cc = CallContext::new(principal, owner_ctx, SessionId::new(), d.kernel_id());

        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({"command": "sleep 30", "background": true}),
                },
                &owner_cc,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let bg_id = start.structured.unwrap()["background_id"].as_str().unwrap().to_string();

        // A no-exec context that only holds facade:shell (can list/read but
        // must not kill, even setting aside cross-context isolation — test
        // against the SAME owner context using a binding without exec).
        let mut no_exec = ContextToolBinding::new();
        no_exec.grant(Capability::Facade("shell".into()));
        broker.set_binding(owner_ctx, no_exec).await;

        let err = broker
            .call_tool(
                call(BackgroundServer::TOOL_KILL, serde_json::json!({"id": bg_id.clone()})),
                &owner_cc,
                CancellationToken::new(),
            )
            .await;
        assert!(err.is_err(), "kill must be denied without the exec authority");

        // Restore exec and clean up the still-running sleep.
        let mut with_exec = ContextToolBinding::new();
        with_exec.grant(Capability::Facade("shell".into()));
        with_exec.grant(Capability::Exec);
        broker.set_binding(owner_ctx, with_exec).await;
        broker
            .call_tool(call(BackgroundServer::TOOL_KILL, serde_json::json!({"id": bg_id})), &owner_cc, CancellationToken::new())
            .await
            .unwrap();
    }

    /// CHARACTERIZATION: the whole point of `background: true` is that
    /// output is observable in the CRDT block WHILE the process is still
    /// running — not only after it exits. A test that only checked the final
    /// block content would pass even under a regression that buffered
    /// everything and only wrote to the block at process exit; this is the
    /// test that would catch that regression, by requiring a snapshot where
    /// the job is STILL `running` AND already carries the pre-sleep output.
    ///
    /// Entirely through the public MCP tool surface (`shell`,
    /// `list_background_processes`, `read_background_output`) — no internals
    /// reached — so it should survive the kaish-job-system swap unchanged.
    #[tokio::test]
    async fn read_background_output_is_observable_while_still_running() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-live"), None, principal);
        d.block_store().create_document(ctx_id, DocKind::Conversation, None).unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({
                        "command": "echo before-sleep; sleep 1; echo after-sleep",
                        "background": true,
                    }),
                },
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("background start should succeed");
        let bg_id = start.structured.unwrap()["background_id"].as_str().unwrap().to_string();

        // Poll for a moment where the job is STILL `running` and the block
        // already carries the pre-sleep output. The command sleeps a full
        // second between the two echoes, so this window is generous — a
        // slow CI host has ample room; if liveness were broken, this loop
        // spins until the timeout instead of ever seeing both conditions
        // hold at once.
        let poll_start = std::time::Instant::now();
        let mid_read = loop {
            let list = broker
                .call_tool(call(BackgroundServer::TOOL_LIST, serde_json::json!({})), &cc, CancellationToken::new())
                .await
                .unwrap();
            let structured = list.structured.unwrap();
            let is_running = structured
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"].as_str() == Some(bg_id.as_str()))
                .and_then(|e| e["status"].as_str())
                == Some("running");

            let read = broker
                .call_tool(
                    call(BackgroundServer::TOOL_READ, serde_json::json!({"id": bg_id})),
                    &cc,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let text = match read.content.first().unwrap() {
                ToolContent::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            };

            if is_running && text.contains("before-sleep") {
                break text;
            }
            assert!(
                poll_start.elapsed() < std::time::Duration::from_secs(5),
                "never observed the process both `running` AND carrying its pre-sleep \
                 output — liveness appears broken (output only surfaces at exit)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            !mid_read.contains("after-sleep"),
            "the captured mid-run snapshot already had the post-sleep output — the process \
             finished faster than expected, which weakens this test's liveness claim; got: {mid_read:?}"
        );

        wait_for_status(&broker, &cc, &bg_id, "exited", std::time::Duration::from_secs(10)).await;
        let final_read = broker
            .call_tool(
                call(BackgroundServer::TOOL_READ, serde_json::json!({"id": bg_id})),
                &cc,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match final_read.content.first().unwrap() {
            ToolContent::Text(s) => assert!(
                s.contains("before-sleep") && s.contains("after-sleep"),
                "final output must carry both halves, got: {s:?}"
            ),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// CHARACTERIZATION: output bounding, at the MCP tool surface.
    /// `background_exec.rs`'s module docs ("Output bounding") promise a
    /// bounded, loudly-marked cap on combined stdout+stderr — this is the
    /// public-surface twin of
    /// `background_exec::tests::output_cap_truncates_with_a_loud_marker_and_still_records_exit`,
    /// which pins the same contract directly against `spawn_background`.
    /// `DEFAULT_OUTPUT_CAP` (an internal constant, unlikely to survive the
    /// kaish-job-system swap as-is) is reached ONLY to size the test's input
    /// past whatever cap the replacement uses; every assertion below is on
    /// `read_background_output`'s returned text and the job's terminal
    /// status, both public.
    #[tokio::test]
    async fn background_output_cap_is_observable_via_read_background_output() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-cap"), None, principal);
        d.block_store().create_document(ctx_id, DocKind::Conversation, None).unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let cap = crate::background_exec::DEFAULT_OUTPUT_CAP;
        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({
                        "command": format!("yes x | head -c {}", cap * 3),
                        "background": true,
                    }),
                },
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("background start should succeed");
        let bg_id = start.structured.unwrap()["background_id"].as_str().unwrap().to_string();

        wait_for_status(&broker, &cc, &bg_id, "exited", std::time::Duration::from_secs(15)).await;

        // Drain the whole (capped) buffer via the same offset-polling
        // contract `read_background_output_polls_losslessly_past_one_read_bound`
        // exercises, to assert on the reassembled text as a caller would see it.
        let mut offset = 0u64;
        let mut assembled = String::new();
        loop {
            let read = broker
                .call_tool(
                    call(BackgroundServer::TOOL_READ, serde_json::json!({"id": bg_id, "offset": offset})),
                    &cc,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let chunk = match read.content.first().unwrap() {
                ToolContent::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            let structured = read.structured.unwrap();
            let next = structured["next_offset"].as_u64().unwrap();
            assembled.push_str(&chunk);
            offset = next;
            if !structured["has_more"].as_bool().unwrap() {
                break;
            }
        }

        assert!(
            assembled.contains("capped"),
            "reassembled output must carry the loud truncation marker, got len={}",
            assembled.len()
        );
        assert!(
            assembled.len() < cap * 2,
            "reassembled output must not grow unbounded past the cap, got {} bytes",
            assembled.len()
        );

        // Exit status must still be recorded even though most output was
        // discarded — the cap must never starve the pipe reader.
        let entry = wait_for_status(&broker, &cc, &bg_id, "exited", std::time::Duration::from_secs(1)).await;
        assert_eq!(entry["exit_code"], serde_json::json!(0), "exit status must survive the cap");
    }

    /// CHARACTERIZATION: process-group kill. `background_exec.rs` spawns the
    /// backgrounded `/bin/sh -c <command>` as the leader of its own new
    /// process group specifically so that killing it reaches the WHOLE tree,
    /// not just the direct child — a command that forks a grandchild (e.g.
    /// `cmd &` inside the backgrounded shell) must have that grandchild
    /// killed too when `kill_background_process` fires.
    ///
    /// Verified via a live heartbeat file the grandchild rewrites every
    /// ~50ms, rather than checking process existence — a killed process
    /// briefly remains visible as a zombie until reaped, which would make an
    /// existence check flaky. A heartbeat that stops advancing is
    /// unambiguous proof the grandchild actually stopped running.
    #[tokio::test]
    async fn kill_background_process_kills_the_whole_process_tree_not_just_the_direct_child() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-tree"), None, principal);
        d.block_store().create_document(ctx_id, DocKind::Conversation, None).unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let heartbeat = std::env::temp_dir().join(format!("kaijutsu-bg-tree-hb-{}", uuid::Uuid::new_v4()));
        let hb_path = heartbeat.display().to_string();

        // The grandchild (`while true; do ...; done`) is backgrounded with
        // `&` INSIDE the shell `spawn_background` starts — it never calls
        // setpgid itself, so it inherits the parent shell's process group.
        // The top-level shell then blocks on `sleep 30` so the whole tree is
        // still alive when we issue the kill.
        let start = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(ShellServer::INSTANCE),
                    tool: ShellServer::TOOL.to_string(),
                    arguments: serde_json::json!({
                        "command": format!(
                            "(while true; do date +%s%N > {hb_path}; sleep 0.05; done) & sleep 30"
                        ),
                        "background": true,
                    }),
                },
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("background start should succeed");
        let bg_id = start.structured.unwrap()["background_id"].as_str().unwrap().to_string();

        // Wait for the heartbeat to actually be advancing (two distinct
        // reads) before we trust it's live.
        let poll_start = std::time::Instant::now();
        let mut last = None;
        loop {
            if let Ok(content) = std::fs::read_to_string(&heartbeat)
                && !content.is_empty()
            {
                match &last {
                    Some(prev) if prev != &content => break,
                    Some(_) => {}
                    None => last = Some(content),
                }
            }
            assert!(
                poll_start.elapsed() < std::time::Duration::from_secs(5),
                "grandchild heartbeat file never started advancing"
            );
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }

        let killed = broker
            .call_tool(
                call(BackgroundServer::TOOL_KILL, serde_json::json!({"id": bg_id})),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("kill should succeed");
        assert!(!killed.is_error);
        wait_for_status(&broker, &cc, &bg_id, "killed", std::time::Duration::from_secs(5)).await;

        // Give the grandchild a moment to actually receive and act on the
        // signal (should be near-instant), then confirm the heartbeat has
        // genuinely stopped advancing over a real window — not just that we
        // caught it between writes.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let after_kill = std::fs::read_to_string(&heartbeat).unwrap_or_default();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let still_after = std::fs::read_to_string(&heartbeat).unwrap_or_default();
        assert_eq!(
            after_kill, still_after,
            "the grandchild kept writing its heartbeat after the top-level process was \
             killed — process-group kill did not reach it"
        );

        let _ = std::fs::remove_file(&heartbeat);
    }
}
