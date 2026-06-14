//! `ShellServer` — the in-kernel projection of the `shell` facade as a broker
//! MCP tool (`builtin.shell` / `shell`).
//!
//! The `shell` facade was historically reachable only over the RPC seam: the
//! human shell box and the external MCP `context_shell` (both cross
//! `Broker::check_facade`). The in-kernel LLM agent's tool roster is built from
//! broker tools (`list_visible_tools`), which never included facades — so a
//! native agent in any context "had no shell" no matter what its binding said.
//!
//! This server closes that gap. It exposes one `shell` tool that materializes
//! the SAME per-context kaish (`KjDispatcher::materialize_context_kaish`) the
//! RPC seam and the rc lifecycle use, so durable env/cwd stay coherent across
//! every surface — there is one shell, reached three ways.
//!
//! Gating stays single-axis: `builtin.shell` is a *facade-projected* instance
//! (see [`crate::mcp::binding::FACADE_PROJECTED_INSTANCES`]), so a context sees
//! and can call `shell` exactly when its binding grants `facade:shell` — the
//! same bit that gates the RPC seam. There is no second capability to keep in
//! sync, and no rc-script change: every role that already had `facade:shell`
//! (default/coder/mcp via `facade:*`, director/composer explicitly) gets the
//! tool; `explorer` (no facade) stays excluded.

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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellParams {
    /// kaish command to run in your current kernel context.
    pub command: String,
}

const DESCRIPTION: &str = "Run a command in your current kernel context using \
    kaish (会sh), a Bourne-like shell with guardrails: no word splitting ($VAR \
    is always one argument — use `split` to split), strict globs (zero matches \
    is an error, not a silent pass-through), `case ... esac` instead of `test \
    \"$x\" = ...`, and pre-validation (syntax errors are caught before anything \
    runs, so a command never half-executes). `kj` is in scope for \
    context/drift/fork management; builtins accept --json for structured \
    output. Returns combined stdout (stderr appended when present); a nonzero \
    exit code is reported as an error.";

const DESCRIPTION_READ_ONLY: &str = "Run a READ-ONLY command in your current \
    kernel context using kaish (会sh). Same Bourne-like shell with guardrails \
    (no word splitting, strict globs, `case ... esac`, pre-validation), but \
    this shell cannot mutate anything: every file write/delete/move and every \
    external command is refused. Use it to inspect — read files, `grep`, \
    `find`, walk the tree, and read the CRDT document/input views under \
    `/v/docs` and `/v/input`; `kj` is in scope for read-only context \
    introspection. Returns combined stdout (stderr appended when present); a \
    nonzero exit code is reported as an error.";

/// In-kernel broker server backing the `shell` / `read_only_shell` tool. Holds
/// `Weak<Broker>` (the broker owns this instance's `Arc`) and reaches the
/// shared `KjDispatcher` through the broker, materializing a throwaway context
/// kaish per call. One struct, two flavours selected at construction: the
/// writable `shell` (`facade:shell`) and the read-only `read_only_shell`
/// (`facade:shell_readonly`) the explorer gets. The constraint lives in the
/// *tool name* so the model never wastes a turn attempting a write it can't do.
pub struct ShellServer {
    instance_id: InstanceId,
    /// The model-facing tool name (`shell` or `read_only_shell`).
    tool: &'static str,
    /// When true, materialize a read-only context kaish (no writes, no external
    /// commands; reads — incl. CRDT views — still work).
    read_only: bool,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl ShellServer {
    pub const INSTANCE: &'static str = "builtin.shell";
    pub const TOOL: &'static str = "shell";
    pub const INSTANCE_READ_ONLY: &'static str = "builtin.shell_readonly";
    pub const TOOL_READ_ONLY: &'static str = "read_only_shell";

    /// The writable `shell` tool (gated by `facade:shell`).
    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            tool: Self::TOOL,
            read_only: false,
            broker,
            notif_tx,
        }
    }

    /// The read-only `read_only_shell` tool (gated by `facade:shell_readonly`).
    pub fn new_read_only(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE_READ_ONLY),
            tool: Self::TOOL_READ_ONLY,
            read_only: true,
            broker,
            notif_tx,
        }
    }

    fn description(&self) -> &'static str {
        if self.read_only {
            DESCRIPTION_READ_ONLY
        } else {
            DESCRIPTION
        }
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }
}

#[async_trait]
impl McpServerLike for ShellServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let schema = schemars::schema_for!(ShellParams);
        Ok(vec![KernelTool {
            instance: self.instance_id.clone(),
            name: self.tool.to_string(),
            description: Some(self.description().to_string()),
            input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
        }])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool != self.tool {
            return Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: params.tool,
            });
        }
        let parsed: ShellParams =
            serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;

        // Reach the shared dispatcher (wired at bootstrap via
        // `Broker::set_kj_dispatcher`) and materialize the SAME per-context
        // kaish the RPC seam and rc lifecycle use. Kernel-side callers pass no
        // semantic index + a no-op block source, so `kj`'s synthesis/search
        // tools are degraded here (matching rc/hooks); the core `kj` verbs and
        // shell work. Wiring the real index is a follow-up.
        let broker = self.broker()?;
        let dispatcher = broker
            .kj_dispatcher()
            .await
            .ok_or_else(|| McpError::InstanceDown {
                instance: self.instance_id.clone(),
                reason: "kj dispatcher not wired (Broker::set_kj_dispatcher)".to_string(),
            })?;

        // Pair the kernel's semantic index with a block-backed source so the
        // model's `kj search`/synthesis tools work inside the shell. Both come
        // from the dispatcher (the server installs the index at bootstrap);
        // when embeddings aren't configured the index is `None` and `kj` falls
        // back to non-semantic search rather than failing.
        let semantic_index = dispatcher.semantic_index();
        let block_source = dispatcher.block_source();
        let kaish = if self.read_only {
            dispatcher
                .materialize_context_kaish_read_only(
                    "model-shell-ro",
                    ctx.principal_id,
                    ctx.context_id,
                    ctx.session_id,
                    semantic_index,
                    block_source,
                )
                .await
        } else {
            dispatcher
                .materialize_context_kaish(
                    "model-shell",
                    ctx.principal_id,
                    ctx.context_id,
                    ctx.session_id,
                    semantic_index,
                    block_source,
                )
                .await
        }
        .map_err(|e| McpError::Protocol(format!("materialize context shell: {e}")))?;

        let result = kaish
            .execute_with_options(&parsed.command, kaish_kernel::ExecuteOptions::default())
            .await
            .map_err(|e| McpError::Protocol(format!("shell execution failed: {e}")))?;

        Ok(shell_result_to_kernel(result))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

/// Collapse a kaish `ExecResult` onto the D-28 `is_error` channel. stdout is
/// the model-facing body; stderr is appended when present so a
/// successful-with-warnings command (exit 0 + stderr) still surfaces it, and a
/// nonzero exit is both flagged (`is_error`) and labelled in the body. A
/// structured envelope carries the exit code + raw streams for programmatic
/// consumers.
fn shell_result_to_kernel(result: kaish_kernel::interpreter::ExecResult) -> KernelToolResult {
    let stdout = result.text_out().into_owned();
    let stderr = result.err.clone();
    let exit_code = result.code;
    let is_error = exit_code != 0;
    // kj verbs (and any builtin that opts in) attach a structured `.data`
    // payload — context-id arrays for list commands, records for inspect. Carry
    // it into the structured envelope so programmatic consumers don't scrape
    // stdout. `null` when the command set no data (external commands, echo, …).
    let data = result
        .data
        .as_ref()
        .map(kaish_kernel::interpreter::value_to_json);

    let mut body = stdout.clone();
    let mut push_line = |s: &str| {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(s);
    };
    if !stderr.is_empty() {
        push_line(&stderr);
    }
    if is_error {
        push_line(&format!("[exit {exit_code}]"));
    }

    KernelToolResult {
        is_error,
        content: vec![ToolContent::Text(body)],
        structured: Some(serde_json::json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "data": data,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{register_context, test_dispatcher};
    use crate::mcp::binding::{Capability, ContextToolBinding};
    use crate::mcp::{InstancePolicy, KernelCallParams};
    use kaijutsu_types::{PrincipalId, SessionId};

    /// An `Arc<KjDispatcher>` wired into a fresh broker with BOTH the writable
    /// and read-only `ShellServer`s registered — the runtime shape
    /// (`set_self_arc` + `set_kj_dispatcher`), so facade gating across the two
    /// can be exercised together.
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
                Arc::new(ShellServer::new_read_only(Arc::downgrade(&broker))),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        (broker, d)
    }

    fn call_ro(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_READ_ONLY),
            tool: ShellServer::TOOL_READ_ONLY.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    fn call(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// End-to-end through `broker.call_tool`: `facade:shell` alone (no `*`, no
    /// instance grant) must let the model run a command. This is the whole
    /// point — facade-only loadouts (director/composer) get a working shell.
    #[tokio::test]
    async fn facade_shell_runs_a_command_through_the_broker() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("sh"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hello-shell"), &cc, CancellationToken::new())
            .await
            .expect("shell call should succeed");

        assert!(!result.is_error, "echo should not be an error");
        match result.content.first().expect("content") {
            ToolContent::Text(s) => {
                assert!(s.contains("hello-shell"), "stdout missing, got: {s:?}")
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// A `kj` verb's structured `.data` must survive into the tool result's
    /// `structured` envelope — consumers read full context handles from `data`
    /// instead of scraping stdout (which renders short ids in a table).
    #[tokio::test]
    async fn kj_data_payload_reaches_structured_envelope() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("alpha"), None, principal);
        register_context(&d, Some("beta"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("kj context list"), &cc, CancellationToken::new())
            .await
            .expect("kj context list should succeed");

        assert!(!result.is_error, "kj context list errored: {result:?}");
        let structured = result.structured.expect("structured envelope present");
        let data = structured
            .get("data")
            .and_then(|d| d.as_array())
            .unwrap_or_else(|| panic!("data must be a JSON array, got: {structured}"));
        let labels: Vec<&str> = data.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            labels.contains(&"alpha") && labels.contains(&"beta"),
            "structured data must carry context handles: {labels:?}"
        );
    }

    /// An `echo` (no structured data) leaves `data` null — the field is present
    /// but empty, never fabricated.
    #[tokio::test]
    async fn plain_command_leaves_data_null() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("sh"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hi"), &cc, CancellationToken::new())
            .await
            .expect("echo should succeed");
        let structured = result.structured.expect("structured envelope present");
        assert!(
            structured.get("data").is_some_and(|d| d.is_null()),
            "echo must leave data null, got: {structured}"
        );
    }

    /// Deny-by-default: a context WITHOUT `facade:shell` (here a read-only-ish
    /// loadout) must be refused at the broker capability gate — the projection
    /// is the only path to the tool, so no facade means no shell.
    #[tokio::test]
    async fn no_facade_is_denied_at_the_gate() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("noshell"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Tool {
            instance: InstanceId::new("builtin.file"),
            tool: "read".to_string(),
        });
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let err = broker
            .call_tool(call("echo nope"), &cc, CancellationToken::new())
            .await
            .expect_err("must be denied without facade:shell");
        assert!(
            matches!(err, McpError::CapabilityDenied { .. }),
            "expected CapabilityDenied, got {err:?}"
        );
    }

    /// The tool must be advertised to a `facade:shell` context (so it lands in
    /// the model's roster + `<tools>` system-prompt line) and hidden otherwise.
    #[tokio::test]
    async fn tool_is_listed_only_with_the_facade() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();

        let with = register_context(&d, Some("with"), None, principal);
        let mut b = ContextToolBinding::new();
        b.grant(Capability::Facade("shell".into()));
        broker.set_binding(with, b).await;
        let cc = CallContext::new(principal, with, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(with, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell context should see the shell tool: {visible:?}"
        );

        let without = register_context(&d, Some("without"), None, principal);
        broker.set_binding(without, ContextToolBinding::new()).await;
        let cc2 = CallContext::new(principal, without, SessionId::new(), d.kernel_id());
        let visible2 = broker.list_visible_tools(without, &cc2).await.unwrap();
        assert!(
            !visible2.iter().any(|(name, _)| name == "shell"),
            "no-facade context must not see the shell tool: {visible2:?}"
        );
    }

    #[test]
    fn conversion_success_with_warnings_keeps_exit_zero_and_surfaces_stderr() {
        let mut r = kaish_kernel::interpreter::ExecResult::success("the-output");
        r.err = "a-warning".to_string();
        let kr = shell_result_to_kernel(r);
        assert!(!kr.is_error, "exit 0 stays non-error even with stderr");
        match kr.content.first().unwrap() {
            ToolContent::Text(s) => {
                assert!(s.contains("the-output"));
                assert!(s.contains("a-warning"), "stderr must be surfaced: {s:?}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn conversion_nonzero_exit_is_error_and_labelled() {
        let r = kaish_kernel::interpreter::ExecResult::failure(3, "boom");
        let kr = shell_result_to_kernel(r);
        assert!(kr.is_error, "nonzero exit must be an error");
        match kr.content.first().unwrap() {
            ToolContent::Text(s) => {
                assert!(s.contains("boom"));
                assert!(s.contains("[exit 3]"), "exit code must be labelled: {s:?}");
            }
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(
            kr.structured.unwrap()["exit_code"],
            serde_json::json!(3),
            "structured envelope carries the exit code"
        );
    }

    /// The explorer's loadout: `facade:shell_readonly` (and NOT `facade:shell`).
    /// It must see exactly the `read_only_shell` tool and NOT the writable
    /// `shell` — one shell or the other, never both, for a narrow role.
    #[tokio::test]
    async fn read_only_role_sees_only_the_read_only_shell() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ro"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_readonly".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "read_only_shell"),
            "facade:shell_readonly must expose read_only_shell: {visible:?}"
        );
        assert!(
            !visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell_readonly must NOT expose the writable shell: {visible:?}"
        );
    }

    /// The mirror: a `facade:shell` (writable) role sees `shell` and NOT
    /// `read_only_shell`. Together with the test above, this is the "one shell
    /// or the other" invariant for the narrow roles.
    #[tokio::test]
    async fn writable_role_does_not_see_the_read_only_shell() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("rw"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell must expose the writable shell: {visible:?}"
        );
        assert!(
            !visible.iter().any(|(name, _)| name == "read_only_shell"),
            "facade:shell must NOT expose read_only_shell: {visible:?}"
        );
    }

    /// End-to-end through `broker.call_tool`: `facade:shell_readonly` lets the
    /// model run a *read* command and get its output. Refusal of writes /
    /// external commands is enforced structurally and unit-tested at the
    /// `MountBackend` / `ReadOnlyFs` layers; here we prove the gate opens for a
    /// read and the command actually runs in the read-only materialization.
    #[tokio::test]
    async fn read_only_shell_runs_a_read_command() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("roexec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_readonly".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call_ro("echo hello-ro"), &cc, CancellationToken::new())
            .await
            .expect("read_only_shell call should succeed");

        assert!(!result.is_error, "echo should not be an error: {result:?}");
        match result.content.first().expect("content") {
            ToolContent::Text(s) => {
                assert!(s.contains("hello-ro"), "stdout missing, got: {s:?}")
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }
}
