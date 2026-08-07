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
//! (default/coder/mcp via `facade:*`, director explicitly) gets the tool;
//! `toolie` holds `facade:shell_readonly` and so gets the read-only twin;
//! `musician` holds neither and is excluded by design — its binding grants
//! only `drive`, because a small local model plays best with an empty tool
//! palette (see `assets/defaults/rc/musician/create/S10-binding.kai`).

use std::sync::{Arc, LazyLock, Weak};

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
    /// Optional standard input fed to the first stdin-reading command in
    /// `command` (e.g. `jq '.name'`, `grep foo`, `patch`). Lets you pipe a
    /// payload you already have — a generated document, a block's text — into a
    /// pipeline without first writing a temp file. A command that reads no
    /// stdin ignores it.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Run `command` in the background instead of waiting for it to finish.
    /// Returns immediately with a `background_id` + the `block_id` its
    /// output streams into — never the full output. Poll with
    /// `read_background_output`, list with `list_background_processes`, stop
    /// with `kill_background_process` (same server, `builtin.background`).
    ///
    /// A backgrounded command runs as `/bin/sh -c <command>` directly on the
    /// host — NOT through kaish — so shell syntax (`|`, `&&`, `>`) works but
    /// `kj` verbs and kaish variables do not; use the foreground `shell` for
    /// those. Requires the `exec` authority (same as any external command in
    /// the foreground shell) and is never available on `read_only_shell`.
    #[serde(default)]
    pub background: bool,
}

// The kaish-language guidance (word-splitting, globs, `case`/`esac`,
// pre-validation, …) is composed from `kaish-help` at process start instead of
// hand-maintained here — that crate exists so a kaish release updates this
// text everywhere (kaijutsu, kaibo) instead of every embedder re-drifting its
// own prose (kaish's `docs/composable-help.md` step 4). `without_overlay()`
// drops the copy-on-write-overlay paragraph: kaijutsu materializes a fresh
// context kaish per call and never turns overlay on, so that guidance would
// be an active mixed signal ("run `kaish-vfs commit`" for a mode that isn't
// enabled). `LazyLock`, not `const`, because composition is a runtime call
// (`compose()`), not a `&'static str` kaish-help can hand us at compile time.
static COMPOSED_TOOL_DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    kaish_help::compose(
        &kaish_help::Recipe::tool_description().without_overlay(),
        &kaish_help::SchemaContent::new(&[]),
    )
});

/// The composed (kaish-sourced) half of the shell tool description, for the
/// cross-slot duplication guard in `kj::kaish` — the primer must not repeat
/// what already rides here. Exposed rather than duplicated so the guard reads
/// the real bytes, not a second composition that could drift from this one.
///
/// `cfg(test)` rather than `allow(dead_code)`: it exists for the guard, and a
/// production build has no caller.
#[cfg(test)]
pub(crate) fn composed_tool_description() -> &'static str {
    &COMPOSED_TOOL_DESCRIPTION
}

// The kaijutsu-specific half kaish-help can't know: what this tool IS here
// (runs in the caller's current kernel context), that `kj` is in scope for
// context/drift/fork management, and the return contract (combined stdout,
// stderr appended, nonzero exit reported as an error). Kept as an intro
// paragraph, separated from the composed kaish-language rules by a blank
// line, so the two sources stay visibly distinct rather than blurring into
// one hand-tuned paragraph the way the old static file did.
static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a command in your current kernel context using kaish (会sh). \
         `kj` is in scope for context/drift/fork management. Returns \
         combined stdout (stderr appended when present); a nonzero exit \
         code is reported as an error.\n\n{}",
        &*COMPOSED_TOOL_DESCRIPTION
    )
});

// Read-only variant's kaijutsu-specific half: same return contract, plus what
// makes it read-only (no mutation, no external commands) and the CRDT views
// it can still read (`/v/docs`, `/v/input`) that a host-only read-only shell
// wouldn't have.
static DESCRIPTION_READ_ONLY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a READ-ONLY command in your current kernel context using kaish \
         (会sh). This shell cannot mutate anything: every file write/delete/\
         move and every external command is refused. Use it to inspect — \
         read files, `grep`, `find`, walk the tree, and read the CRDT \
         document/input views under `/v/docs` and `/v/input`; `kj` is in \
         scope for read-only context introspection. Returns combined stdout \
         (stderr appended when present); a nonzero exit code is reported as \
         an error.\n\n{}",
        &*COMPOSED_TOOL_DESCRIPTION
    )
});

/// In-kernel broker server backing the `shell` / `read_only_shell` tool. Holds
/// `Weak<Broker>` (the broker owns this instance's `Arc`) and reaches the
/// shared `KjDispatcher` through the broker, materializing a throwaway context
/// kaish per call. One struct, two flavours selected at construction: the
/// writable `shell` (`facade:shell`) and the read-only `read_only_shell`
/// (`facade:shell_readonly`) the toolie gets. The constraint lives in the
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
            &DESCRIPTION_READ_ONLY
        } else {
            &DESCRIPTION
        }
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }

    /// `background: true` path — bypasses the kaish materialization entirely
    /// (see `background_exec.rs` module docs for why: a per-call kaish
    /// instance can't host a registry that outlives the call, and kaish's own
    /// external-command capture isn't live). Spawns `command` as a direct
    /// host process, streaming into a fresh `Running` block, and returns as
    /// soon as it's registered — never the command's output.
    async fn start_background(
        &self,
        command: &str,
        dispatcher: &crate::kj::KjDispatcher,
        ctx: &CallContext,
    ) -> McpResult<KernelToolResult> {
        // `read_only_shell` is structurally read-only (its materialized kaish
        // pins `ExternalExec::Deny`) — background execution must be refused
        // the same way, by construction, not left to the exec-authority
        // check below (which a read-only role never holds anyway, but this
        // keeps the refusal reason specific to the tool rather than a
        // generic capability-denied).
        if self.read_only {
            return Err(McpError::Protocol(
                "read_only_shell cannot start background processes (it never spawns host subprocesses)"
                    .to_string(),
            ));
        }

        // Same authority a synchronous external command requires — `exec` —
        // never a weaker gate. `facade:shell` alone (which `builtin.background`
        // rides too, see FACADE_PROJECTED_INSTANCES) only grants kj/builtins;
        // spawning a real host process needs the dedicated `exec` authority on
        // top, exactly like `ExternalExec::Allow` vs `Deny` in
        // `kj/context_shell.rs`.
        let broker = self.broker()?;
        let exec_granted = broker
            .binding(&ctx.context_id)
            .await
            .is_some_and(|b| b.allows(&crate::mcp::Capability::Exec));
        if !exec_granted {
            return Err(McpError::Protocol(
                "background execution requires the `exec` authority (deny-by-default — see `kj binding allow exec`)"
                    .to_string(),
            ));
        }

        let kernel = dispatcher.kernel();
        let kernel_db = dispatcher.kernel_db();

        // cwd: mirror the synchronous shell's persisted `context_shell.cwd`,
        // but validated as a REAL host directory. A background spawn goes
        // straight to the host (bypassing kaish's VFS), so a virtual-only cwd
        // like `/v/docs` can't be honored — surfaced as an error rather than
        // silently landing somewhere else.
        let persisted_cwd = {
            let db = kernel_db.lock();
            db.get_context_shell(ctx.context_id)
                .ok()
                .flatten()
                .and_then(|row| row.cwd)
        };
        let cwd = match persisted_cwd {
            Some(p) => {
                let path = std::path::PathBuf::from(&p);
                if path.is_dir() {
                    path
                } else {
                    return Err(McpError::Protocol(format!(
                        "background execution needs a host-real cwd; this context's cwd ({p}) doesn't resolve on the host filesystem"
                    )));
                }
            }
            None => kaish_kernel::home_dir(),
        };

        // env: hermetic like the synchronous shell — PATH is the kernel's
        // startup capture, HOME is seeded, and the context's durable env vars
        // are exported (mirrors `EmbeddedKaish::apply_context_config`).
        let mut env = vec![(
            "HOME".to_string(),
            kaish_kernel::home_dir().to_string_lossy().into_owned(),
        )];
        if let Some(path) = kernel.host_path() {
            env.push(("PATH".to_string(), path.to_string()));
        }
        {
            let db = kernel_db.lock();
            if let Ok(vars) = db.get_context_env(ctx.context_id) {
                for v in vars {
                    env.push((v.key, v.value));
                }
            }
        }

        let blocks = dispatcher.block_store();
        let block_id = blocks
            .insert_block_as(
                ctx.context_id,
                None,
                None,
                kaijutsu_crdt::Role::Tool,
                kaijutsu_crdt::BlockKind::ToolResult,
                String::new(),
                kaijutsu_crdt::Status::Running,
                kaijutsu_crdt::ContentType::Plain,
                Some(ctx.principal_id),
            )
            .map_err(|e| McpError::Protocol(format!("failed to create background output block: {e}")))?;

        let registry = kernel.background_processes();
        let bg_id = crate::background_exec::spawn_background(
            registry,
            blocks,
            crate::background_exec::SpawnBackgroundParams {
                command: command.to_string(),
                cwd,
                env,
                context_id: ctx.context_id,
                principal_id: ctx.principal_id,
                block_id,
            },
        )
        .map_err(|e| McpError::Protocol(format!("failed to start background process: {e}")))?;

        Ok(KernelToolResult {
            is_error: false,
            content: vec![ToolContent::Text(format!(
                "started background process {bg_id}; output streams into block {}. \
                 Poll with read_background_output, list with list_background_processes, \
                 stop with kill_background_process (builtin.background).",
                block_id.to_key()
            ))],
            structured: Some(serde_json::json!({
                "background_id": bg_id.to_string(),
                "block_id": block_id.to_key(),
                "status": "running",
            })),
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
        if parsed.background {
            return self.start_background(&parsed.command, &dispatcher, ctx).await;
        }

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

        let mut opts = kaish_kernel::ExecuteOptions::default();
        if let Some(stdin) = parsed.stdin {
            opts = opts.with_stdin(stdin);
        }
        let result = kaish
            .execute_with_options(&parsed.command, opts)
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
/// consumers, plus any confirmation-latch request so a caller can fulfill it
/// structurally rather than parsing the prose.
///
/// A capped result (kaish `did_spill`: exit remapped to 3, real exit stashed
/// in `original_code`) is judged by the command's REAL exit — truncation is
/// not failure, and an `is_error` here tempts a model into re-running a
/// command that already succeeded. The truncation itself stays unmissable:
/// `[output truncated]` in the body, `did_spill` in the envelope.
fn shell_result_to_kernel(result: kaish_kernel::interpreter::ExecResult) -> KernelToolResult {
    let stdout = result.text_out().into_owned();
    let stderr = result.err.clone();
    let exit_code = if result.did_spill {
        result.original_code.unwrap_or(result.code)
    } else {
        result.code
    };
    let is_error = exit_code != 0;
    // kj verbs (and any builtin that opts in) attach a structured `.data`
    // payload — context-id arrays for list commands, records for inspect. Carry
    // it into the structured envelope so programmatic consumers don't scrape
    // stdout. `null` when the command set no data (external commands, echo, …).
    let data = result
        .data
        .as_ref()
        .map(kaish_kernel::interpreter::value_to_json);
    // A confirmation latch (exit 2, e.g. `kj context remove` or `rm` under
    // `set -o latch`) rides its typed request on the control-plane `.latch`
    // field (kaish 0.11), distinct from the data-plane `.data`. Surface it so a
    // batch loop reads `structured.latch.nonce`/`.hint` and re-runs with
    // `--confirm=<nonce>`, instead of scraping the confirmation prose out of the
    // body. `null` when the command didn't latch.
    let latch = result.latch_request();

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
    if result.did_spill {
        push_line("[output truncated]");
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
            "did_spill": result.did_spill,
            "data": data,
            "latch": latch,
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

    /// The composed half must carry real kaish-help content (a known rule)
    /// and must NOT carry the overlay paragraph — the assertion that would
    /// have caught shipping published kaish-help 0.13 (which forces overlay
    /// guidance into every recipe) instead of the opt-in-overlay rev this
    /// dependency is pinned to.
    #[test]
    fn composed_tool_description_has_a_known_rule_and_excludes_overlay() {
        let text = DESCRIPTION.as_str();
        assert!(
            text.to_lowercase().contains("word splitting"),
            "composed description should carry the no-word-splitting rule: {text}"
        );
        assert!(
            !text.contains("Overlay mode") && !text.contains("kaish-vfs commit"),
            "kaijutsu never enables overlay mode; the description must not tell \
             the model to run `kaish-vfs commit`: {text}"
        );

        let ro_text = DESCRIPTION_READ_ONLY.as_str();
        assert!(
            ro_text.to_lowercase().contains("word splitting"),
            "read-only description should carry the same composed rules: {ro_text}"
        );
        assert!(
            !ro_text.contains("Overlay mode") && !ro_text.contains("kaish-vfs commit"),
            "read-only description must not carry overlay guidance either: {ro_text}"
        );
    }

    /// The kaijutsu-specific wrapper — what kaish-help can't know — must
    /// survive composition: what the tool IS here (current kernel context),
    /// `kj` in scope, and the return contract. The read-only variant also
    /// names its mutation refusal and the CRDT views it can still read.
    #[test]
    fn kaijutsu_wrapper_survives_composition() {
        let text = DESCRIPTION.as_str();
        assert!(text.contains("current kernel context"), "{text}");
        assert!(text.contains("`kj` is in scope"), "{text}");
        assert!(
            text.contains("combined stdout") && text.contains("nonzero exit"),
            "return contract must survive: {text}"
        );

        let ro_text = DESCRIPTION_READ_ONLY.as_str();
        assert!(
            ro_text.contains("cannot mutate anything"),
            "read-only contract must survive: {ro_text}"
        );
        assert!(
            ro_text.contains("/v/docs") && ro_text.contains("/v/input"),
            "read-only CRDT views must survive: {ro_text}"
        );
        assert!(
            ro_text.contains("combined stdout") && ro_text.contains("nonzero exit"),
            "return contract must survive on the read-only variant too: {ro_text}"
        );
    }

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
    /// point — facade-only loadouts (director/musician) get a working shell.
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
    fn conversion_surfaces_latch_request_structurally() {
        // A latched destructive op (exit 2) carries a typed `LatchRequest` on the
        // control-plane `.latch` field (kaish 0.11). The MCP shell envelope must
        // surface it so a batch loop reads the nonce structurally instead of
        // scraping the confirmation prose out of the body. Resolves the on-hold
        // docs/issues.md "latch nonce on stderr" entry.
        let mut r = kaish_kernel::interpreter::ExecResult::failure(
            2,
            "kj context remove: confirmation required\n\
             To confirm, run: kj context remove doomed --confirm abc123",
        );
        r.latch = Some(Box::new(kaish_kernel::interpreter::LatchRequest {
            nonce: "abc123".to_string(),
            command: "kj context remove".to_string(),
            paths: vec!["doomed".to_string()],
            hint: "kj context remove doomed --confirm abc123".to_string(),
            tool: "kj".to_string(),
            argv: vec![
                "context".to_string(),
                "remove".to_string(),
                "doomed".to_string(),
            ],
            ttl: 60,
            job_id: None,
        }));
        let structured = shell_result_to_kernel(r)
            .structured
            .expect("structured envelope");
        assert_eq!(structured["latch"]["nonce"], serde_json::json!("abc123"));
        assert_eq!(
            structured["latch"]["command"],
            serde_json::json!("kj context remove")
        );
        assert_eq!(
            structured["latch"]["hint"],
            serde_json::json!("kj context remove doomed --confirm abc123"),
            "the ready-to-run confirmation command must ride the structured envelope"
        );

        // A non-latched result carries an explicit null — present as a key so a
        // consumer can test `latch == null` rather than guess at omission.
        let plain = shell_result_to_kernel(kaish_kernel::interpreter::ExecResult::success("ok"));
        assert_eq!(
            plain.structured.unwrap()["latch"],
            serde_json::Value::Null,
            "a non-latched result leaves `latch` explicitly null"
        );
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

    #[test]
    fn conversion_spilled_success_is_not_error_but_signals_truncation() {
        // kaish remaps a capped/spilled result to exit 3, stashing the real
        // exit in `original_code`. Truncation is not failure: flagging it
        // `is_error` tempts a model into re-running a command that succeeded.
        // The truncation must still be unmissable — a model reasoning over a
        // head+tail excerpt as if it were complete output hallucinates.
        let mut r = kaish_kernel::interpreter::ExecResult::success(
            "head…\n[output truncated: spilled to /v/spill/abc]",
        );
        r.did_spill = true;
        r.original_code = Some(r.code);
        r.code = 3;
        let kr = shell_result_to_kernel(r);
        assert!(!kr.is_error, "a spilled successful command is not an error");
        match kr.content.first().unwrap() {
            ToolContent::Text(s) => {
                assert!(
                    s.contains("[output truncated]"),
                    "truncation must be labelled in the body: {s:?}"
                );
                assert!(
                    !s.contains("[exit"),
                    "a successful spill must not carry an exit label: {s:?}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }
        let structured = kr.structured.unwrap();
        assert_eq!(
            structured["exit_code"],
            serde_json::json!(0),
            "envelope carries the command's real exit, not kaish's spill marker"
        );
        assert_eq!(structured["did_spill"], serde_json::json!(true));
    }

    #[test]
    fn conversion_spilled_failure_stays_error_with_original_code() {
        let mut r = kaish_kernel::interpreter::ExecResult::failure(3, "tail of a real failure");
        r.did_spill = true;
        r.original_code = Some(1);
        let kr = shell_result_to_kernel(r);
        assert!(kr.is_error, "a spilled FAILING command is still an error");
        match kr.content.first().unwrap() {
            ToolContent::Text(s) => {
                assert!(s.contains("[output truncated]"), "truncation labelled: {s:?}");
                assert!(
                    s.contains("[exit 1]"),
                    "exit label shows the real code, not the spill marker: {s:?}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }
        let structured = kr.structured.unwrap();
        assert_eq!(structured["exit_code"], serde_json::json!(1));
        assert_eq!(structured["did_spill"], serde_json::json!(true));
    }

    #[test]
    fn conversion_unspilled_results_report_did_spill_false() {
        let plain = shell_result_to_kernel(kaish_kernel::interpreter::ExecResult::success("ok"));
        assert_eq!(
            plain.structured.unwrap()["did_spill"],
            serde_json::json!(false),
            "did_spill is always present so consumers can test it directly"
        );
    }

    /// The toolie's loadout: `facade:shell_readonly` (and NOT `facade:shell`).
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

    /// `background: true` requires the `exec` authority on top of
    /// `facade:shell` — the same gate a synchronous external command hits,
    /// never a weaker one. A context with `facade:shell` alone (no `exec`)
    /// must be refused, not silently degrade to a foreground run.
    #[tokio::test]
    async fn background_true_is_denied_without_exec_authority() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-noexec"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({"command": "echo nope", "background": true}),
        };
        let err = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect_err("background execution must be denied without the exec authority");
        assert!(
            matches!(err, McpError::Protocol(_)),
            "expected a Protocol denial explaining the missing exec authority, got {err:?}"
        );
    }

    /// End-to-end: `shell(background: true)` with `facade:shell` + `exec`
    /// returns IMMEDIATELY (a handle + block id, never the command's output),
    /// and the command actually runs — its output shows up in the returned
    /// block a moment later, proving the async path is really wired, not
    /// just accepting the flag and doing nothing.
    #[tokio::test]
    async fn background_true_returns_immediately_and_streams_into_its_block() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-ok"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({"command": "echo streamed-bg-output", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");

        assert!(!result.is_error, "starting a background process is not itself an error");
        let structured = result.structured.expect("structured envelope");
        assert_eq!(structured["status"], serde_json::json!("running"));
        let block_key = structured["block_id"].as_str().expect("block_id present").to_string();
        assert!(structured["background_id"].as_str().is_some(), "background_id present");
        // The response body must be a short confirmation, never the command's
        // full output — that's the whole point of backgrounding.
        match result.content.first().unwrap() {
            ToolContent::Text(s) => assert!(
                !s.contains("streamed-bg-output"),
                "the immediate response must not carry the command's output, got: {s:?}"
            ),
            other => panic!("expected text, got {other:?}"),
        }

        let block_id = kaijutsu_crdt::BlockId::from_key(&block_key).expect("valid block key");
        let start = std::time::Instant::now();
        loop {
            let snap = d
                .block_store()
                .get_block_snapshot(ctx_id, &block_id)
                .unwrap()
                .expect("block exists");
            if snap.content.contains("streamed-bg-output") {
                break;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for background output to stream in");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// `read_only_shell` must refuse `background: true` outright — it never
    /// spawns host subprocesses by construction (its materialized kaish pins
    /// `ExternalExec::Deny`), and background execution must not be a back
    /// door around that.
    #[tokio::test]
    async fn read_only_shell_rejects_background_true() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ro-bg"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_readonly".into()));
        // Even granting `exec` (which no real read-only role would have)
        // must not open the door — the refusal is structural on `read_only`,
        // checked before the capability gate.
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_READ_ONLY),
            tool: ShellServer::TOOL_READ_ONLY.to_string(),
            arguments: serde_json::json!({"command": "echo nope", "background": true}),
        };
        let err = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect_err("read_only_shell must refuse background execution even with exec granted");
        assert!(matches!(err, McpError::Protocol(_)), "expected a Protocol refusal, got {err:?}");
    }
}
