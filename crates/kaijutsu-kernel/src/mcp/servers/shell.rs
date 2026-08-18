//! `ShellServer` — the in-kernel projection of the `shell` / `shell_write`
//! facades as broker MCP tools (`builtin.shell` / `shell` and
//! `builtin.shell_write` / `shell_write`).
//!
//! **2026-08-17 flag day** (`docs/gate-and-shell-split.md`, "Slice 3", Amy's
//! 2026-08-16 ruling): `shell` is now the unmarked, SAFE name
//! (`ExternalExec::Deny`) — the tool a model reaches for by accident must be
//! the one that cannot hurt anything. `shell_write` is the hot, mutating name
//! (`ExternalExec::Allow`, same behavior `builtin.shell`/`shell` had before
//! the flag day), granted not default. `read_only_shell` retires as a name
//! entirely — no dual-name transition period. A stale caller that still asks
//! for `"shell"` after the flag day lands on the SAFE tool now, never the
//! mutating one — wrong-but-safe, the only acceptable direction for a
//! breaking rename.
//!
//! The `shell`/`shell_write` facades were historically reachable only over the
//! RPC seam: the human shell box and the external MCP `context_shell` (both
//! cross `Broker::check_facade`). The in-kernel LLM agent's tool roster is
//! built from broker tools (`list_visible_tools`), which never included
//! facades — so a native agent in any context "had no shell" no matter what
//! its binding said.
//!
//! This server closes that gap. It exposes tools that materialize the SAME
//! per-context kaish (`KjDispatcher::materialize_context_kaish`) the RPC seam
//! and the rc lifecycle use, so durable env/cwd stay coherent across every
//! surface — there is one shell (per flavor), reached three ways.
//!
//! Gating stays single-axis per flavor: `builtin.shell` and `builtin.
//! shell_write` are each *facade-projected* instances (see
//! [`crate::mcp::binding::FACADE_PROJECTED_INSTANCES`]), so a context sees and
//! can call `shell` exactly when its binding grants `facade:shell`, and
//! `shell_write` exactly when it grants `facade:shell_write` — the same bits
//! that gate the RPC seam. There is no second capability to keep in sync.
//! Default grants stay per-rc, decided per context type at the flag day:
//! `default`/`coder`/`mcp` via `facade:*`; `director` explicitly holds both
//! (operator's console — wants the safe tool AND the hot one available);
//! `toolie` holds `facade:shell` only (never `facade:shell_write`), so it gets
//! exactly the safe tool; `musician` holds neither and is excluded by design —
//! its binding grants only `drive`, because a small local model plays best
//! with an empty tool palette (see
//! `assets/defaults/rc/musician/create/S10-binding.kai`).

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
// makes it read-only (no mutation, no external commands) and the document views
// it can still read (`/v/docs`, `/v/input`) that a host-only read-only shell
// wouldn't have.
static DESCRIPTION_READ_ONLY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a READ-ONLY command in your current kernel context using kaish \
         (会sh). This shell cannot mutate anything: every file write/delete/\
         move and every external command is refused. Use it to inspect — \
         read files, `grep`, `find`, walk the tree, and read the kernel \
         document/input views under `/v/docs` and `/v/input`; `kj` is in \
         scope for read-only context introspection. Returns combined stdout \
         (stderr appended when present); a nonzero exit code is reported as \
         an error.\n\n{}",
        &*COMPOSED_TOOL_DESCRIPTION
    )
});

/// In-kernel broker server backing the `shell` / `shell_write` tools. Holds
/// `Weak<Broker>` (the broker owns this instance's `Arc`) and reaches the
/// shared `KjDispatcher` through the broker, materializing a throwaway context
/// kaish per call. One struct, two flavours selected at construction: the
/// hot, mutating `shell_write` (`facade:shell_write`) and the safe, unmarked
/// `shell` (`facade:shell`) — the name a caller reaches for by default, and
/// what `read_only_shell`/`facade:shell_readonly` used to be before the
/// 2026-08-17 flag day. The constraint lives in the *tool name* so the model
/// never wastes a turn attempting a write it can't do.
pub struct ShellServer {
    instance_id: InstanceId,
    /// The model-facing tool name (`shell`, safe, or `shell_write`, hot).
    tool: &'static str,
    /// When true, materialize a read-only context kaish (no writes, no external
    /// commands; reads — incl. document views — still work).
    read_only: bool,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl ShellServer {
    /// The safe, unmarked tool — `ExternalExec::Deny`. This is what
    /// `builtin.shell_readonly`/`read_only_shell` was before the 2026-08-17
    /// flag day (`docs/gate-and-shell-split.md`, "Slice 3"): the name a caller
    /// reaches for by accident must be the one that cannot hurt anything.
    pub const INSTANCE: &'static str = "builtin.shell";
    pub const TOOL: &'static str = "shell";
    /// The hot, mutating tool — `ExternalExec::Allow`, granted not default.
    /// This is what `builtin.shell`/`shell` was before the flag day; a stale
    /// caller that still asks for the bare name `"shell"` now lands on the
    /// SAFE tool above instead, never here.
    pub const INSTANCE_WRITE: &'static str = "builtin.shell_write";
    pub const TOOL_WRITE: &'static str = "shell_write";

    /// The hot `shell_write` tool (gated by `facade:shell_write`).
    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE_WRITE),
            tool: Self::TOOL_WRITE,
            read_only: false,
            broker,
            notif_tx,
        }
    }

    /// The safe, unmarked `shell` tool (gated by `facade:shell`).
    pub fn new_read_only(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            tool: Self::TOOL,
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
                "the safe `shell` tool cannot start background processes (it never spawns host subprocesses; use `shell_write`)"
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
                kaijutsu_types::Role::Tool,
                kaijutsu_types::BlockKind::ToolResult,
                String::new(),
                kaijutsu_types::Status::Running,
                kaijutsu_types::ContentType::Plain,
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
            // NOT gated (`docs/gate-and-shell-split.md`, "Slice 4"): a
            // backgrounded command runs as a direct host subprocess
            // (`start_background`, below), not through kaish, so
            // `plan_program` cannot describe it — there is no kaish source
            // here to plan. Gating this path is separate, unbuilt work; see
            // `kj::shell_gate`'s module docs for the full list of what this
            // gate does and does not cover.
            return self.start_background(&parsed.command, &dispatcher, ctx).await;
        }

        // The hot, mutating tool is gated (`docs/gate-and-shell-split.md`,
        // "Slice 4") — every foreground `shell_write` submission goes
        // through the approval ledger before it runs. The safe `shell` tool
        // (`self.read_only`) is never gated: it cannot mutate anything
        // (`ExternalExec::Deny`), so a gate on it would be pure friction
        // with nothing to protect against.
        //
        // The gate is all-or-nothing per submission (kaish has no
        // per-command interception hook — `kj::shell_gate`'s module docs
        // explain why) and covers exactly what `plan_program` can see: the
        // kaish source text of `parsed.command`. It does NOT cover a
        // program handed to an interpreter as a string argument or over
        // `parsed.stdin`, and it does not cover `start_background` above —
        // see `kj::shell_gate`'s module docs for the full, honest list.
        if !self.read_only {
            let spec = crate::kj::shell_gate::build_shell_gate_spec(&parsed.command)
                .map_err(|e| McpError::Protocol(format!("shell_write: {e} — nothing was run")))?;
            let caller = crate::kj::KjCaller {
                principal_id: ctx.principal_id,
                context_id: Some(ctx.context_id),
                session_id: ctx.session_id,
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            // **The gate's own deadline must fire before the broker's — and
            // the broker's before the client's.**
            //
            // `run_gate` blocks on a human answering from another surface
            // (`kj ledger`). This call runs *inside* a broker `call_tool`,
            // which is itself the answer to an RPC dispatched by a client
            // with its own per-call deadline. One logical wait, enforced at
            // three more hops outside this function, and each outer hop
            // must give up STRICTLY LATER than the one inside it — otherwise
            // the outermost hop (closest to the model) fires first and the
            // gate's honest "nobody answered ask `<id>`, nothing was run"
            // — the ask id a human needs — gets replaced by a generic
            // timeout that knows nothing about the gate. That is exactly
            // what used to happen here: the client's RPC deadline was
            // SHORTER than this function's own wait, so it won the race.
            //
            // This used to be a locally-computed clamp (`min(gate_wait,
            // mcp_call_timeout_default - 5s)`) with a comment ending "if a
            // third caller ever needs this, lift it into one place rather
            // than copying the arithmetic." A third caller (the client RPC
            // hop) did need it, so the arithmetic now lives in exactly one
            // place: `kaijutsu_types::timeout::gate`. `effective_gate_wait()`
            // is this hop's half of that ladder — the broker's `call_tool`
            // cap for this instance (`InstancePolicy::for_kernel_gated`,
            // `gate::BROKER_CALL`) and the client's per-call override
            // (`gate::CLIENT_CALL`, `kaijutsu-client::actor::dispatch_deadline!`)
            // are the other two rungs, ordered by construction and pinned by
            // a test (`gate_ladder_fires_caller_first`) instead of by three
            // independently-tuned numbers agreeing by luck.
            //
            // Fail-closed is preserved regardless of which hop times out;
            // what the ladder protects is the *reason*, which is exactly the
            // fault-reads-as-something-else family this project keeps
            // filing. This is the same hazard `runtime::kj_builtin` solves
            // for gated `kj` verbs with a patient hold (see its comment
            // citing "Gate slice 1a, finding #1" — "passes tests, dies in
            // production"); the MCP path has no such hold, so it leans on
            // the ladder instead.
            let wait = dispatcher.kernel().timeouts().effective_gate_wait();
            let outcome = crate::kj::gate::run_gate(
                dispatcher.kernel_db(),
                &caller,
                spec,
                wait,
                dispatcher.kernel().ledger_flows(),
            )
            .await;
            if !outcome.allowed {
                return Err(McpError::Protocol(format!(
                    "shell_write: approval gate refused [ask {}] ({}): {} — nothing was run",
                    outcome.request_id, outcome.status, outcome.reason
                )));
            }
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
    // A `kj` confirmation gate (exit 2, e.g. `kj context remove`) rides
    // kaish's opaque `baggage` channel, distinct from the data-plane `.data`.
    // Surface it so a batch loop reads `structured.latch.hint` and re-runs
    // with `--confirm`, instead of scraping the confirmation prose out of the
    // body. `null` when the command didn't latch. kaish's own latch — the one
    // that used to hold `rm` here on a typed `.latch` field — is gone as of
    // 0.14, and was never enabled in kaijutsu anyway (`KaishConfig::named`
    // defaults `latch_enabled` off, and we never called `with_latch`).
    let latch = crate::runtime::kj_builtin::latch_from_result(&result).map(|l| {
        serde_json::json!({
            "command": l.command,
            "target": l.target,
            "hint": l.hint,
        })
    });

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
    use crate::kj::test_helpers::{register_context, test_caller, test_dispatcher};
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
    /// names its mutation refusal and the document views it can still read.
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
            "read-only document views must survive: {ro_text}"
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

    /// Params targeting the SAFE, unmarked `shell` tool (`ExternalExec::Deny`)
    /// — the name a stale/default caller reaches for after the flag day.
    fn call(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// Params targeting the HOT, mutating `shell_write` tool
    /// (`ExternalExec::Allow`) — granted not default.
    fn call_write(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// `shell_write` is gated (`docs/gate-and-shell-split.md`, "Slice 4") —
    /// a test that calls it synchronously now leaves a pending approval
    /// ledger ask and must answer its own ask (the way a human running `kj
    /// ledger allow` would) or the call blocks until `gate_wait_timeout`.
    /// Spawn this BEFORE the call it answers, mirroring `kj::gate`'s own
    /// test pattern (`an_answer_from_another_principal_is_honored`).
    fn spawn_gate_answerer(
        db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
        allow: bool,
    ) -> tokio::task::JoinHandle<String> {
        tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let pending = {
                    let db = db.lock();
                    approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap()
                };
                if let Some(row) = pending.into_iter().next() {
                    let db = db.lock();
                    let conn = db.conn_for_ledger();
                    approval_ledger::claim::claim(conn, &row.request_id, b"test-approver").unwrap();
                    approval_ledger::decide::decide(
                        conn,
                        &row.request_id,
                        approval_ledger::decide::DecideInput {
                            allow,
                            decided_by: Some(b"test-approver"),
                            decided_option: Some(if allow { "allow_once" } else { "deny" }),
                            remember_scope: None,
                            auto_reason: None,
                        },
                    )
                    .unwrap();
                    return row.request_id;
                }
            }
            panic!("no pending gate ask appeared within 4s");
        })
    }

    /// End-to-end through `broker.call_tool`: `facade:shell` alone (no `*`, no
    /// instance grant) must let the model run a command through the SAFE tool.
    /// Post-2026-08-17-flag-day, `facade:shell` is the unmarked/safe grant —
    /// this is the tool a caller reaches for by default.
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

    /// The mirror on the hot side: `facade:shell_write` alone must let the
    /// model run a command through the mutating tool, under its new name.
    /// Director explicitly holds both facades post-flag-day (the operator's
    /// console wants the safe tool by default and the hot one available).
    #[tokio::test]
    async fn facade_shell_write_runs_a_command_through_the_broker() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("shw"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let answerer = spawn_gate_answerer(d.kernel_db().clone(), true);
        let result = broker
            .call_tool(call_write("echo hello-shell-write"), &cc, CancellationToken::new())
            .await
            .expect("shell_write call should succeed");
        answerer.await.unwrap();

        assert!(!result.is_error, "echo should not be an error");
        match result.content.first().expect("content") {
            ToolContent::Text(s) => {
                assert!(s.contains("hello-shell-write"), "stdout missing, got: {s:?}")
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// A dispatcher variant with a SHORT `gate_wait_timeout`, for the tests
    /// below that need an unanswered gate to expire quickly rather than
    /// wait the production default.
    async fn wired_with_short_gate_timeout() -> (Arc<Broker>, Arc<crate::kj::KjDispatcher>) {
        let policy = kaijutsu_types::TimeoutPolicy {
            gate_wait_timeout: std::time::Duration::from_millis(300),
            ..kaijutsu_types::TimeoutPolicy::default()
        };
        let d = Arc::new(crate::kj::test_helpers::test_dispatcher_with_timeouts(policy).await);
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

    /// The gate's deadline must be ordered under the broker's, so an
    /// unanswered gate returns the gate's honest reason rather than a generic
    /// MCP timeout. This USED to be a clamp computed by hand right here
    /// (`min(gate_wait, mcp_call_timeout_default - 5s)`); it is now enforced
    /// by the shared `kaijutsu_types::timeout::gate` ladder —
    /// `effective_gate_wait()` on the kernel side, `gate::BROKER_CALL` as
    /// this instance's `InstancePolicy` cap (`for_kernel_gated`, NOT the
    /// generic `mcp_call_timeout_default` every other instance uses).
    ///
    /// Asserts the ordering rather than a literal duration, so retuning
    /// either bound cannot quietly invert it. `gate_ladder_fires_caller_first`
    /// (`kaijutsu-types::timeout`) pins the ladder's constants in isolation;
    /// this is the integration check that the call site here actually reads
    /// through it rather than reintroducing a local clamp.
    #[test]
    fn the_gate_deadline_is_ordered_under_the_broker_call_timeout() {
        let t = kaijutsu_types::TimeoutPolicy::default();
        let effective = t.effective_gate_wait();
        assert!(
            effective < kaijutsu_types::timeout::gate::BROKER_CALL,
            "the gate must resolve BEFORE the broker cancels the call, or its reason is lost \
             (effective gate wait {:?}, broker cap {:?})",
            effective,
            kaijutsu_types::timeout::gate::BROKER_CALL
        );
        assert!(
            kaijutsu_types::timeout::gate::MAX_KERNEL_WAIT
                < kaijutsu_types::timeout::gate::BROKER_CALL,
            "if this fails the ladder became a no-op and this test stopped testing anything — \
             the gate ceiling caught up with (or passed) the broker cap it's supposed to clear"
        );
    }

    /// **Slice 4 spec test.** An uncovered `shell_write` submission escalates
    /// and blocks on `gate_wait_timeout` — proven through the real
    /// `ShellServer` → `build_shell_gate_spec` → `run_gate` wiring, not just
    /// `run_gate` in isolation (`kj::gate`'s own
    /// `an_uncovered_multi_statement_submission_escalates_and_expires`
    /// covers the underlying mechanism).
    #[tokio::test]
    async fn shell_write_with_no_answer_escalates_and_times_out_refusing_the_call() {
        let (broker, d) = wired_with_short_gate_timeout().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("timeout-shw"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let start = std::time::Instant::now();
        let err = broker
            .call_tool(call_write("echo nobody-answers"), &cc, CancellationToken::new())
            .await
            .expect_err("an unanswered gate must refuse, never hang forever or silently run");
        assert!(start.elapsed() >= std::time::Duration::from_millis(300));
        match err {
            McpError::Protocol(msg) => assert!(
                msg.to_lowercase().contains("expired"),
                "an unanswered gate must expire, distinguishably from a hard denial: {msg}"
            ),
            other => panic!("expected a Protocol refusal, got {other:?}"),
        }
    }

    /// **Slice 4 spec test.** A multi-statement `shell_write` submission a
    /// human denies refuses the WHOLE call — the gate runs entirely before
    /// `execute_with_options` is ever called for this submission, so there
    /// is no partial-execution window to prove separately; this asserts the
    /// refusal itself reaches the caller through the real MCP wiring. The
    /// RULE-covered "one denied statement denies the whole ask" composition
    /// is unit-tested directly against `run_gate` in `kj::gate`
    /// (`a_denied_statement_among_several_refuses_the_whole_submission_and_names_it`).
    #[tokio::test]
    async fn shell_write_deny_refuses_the_whole_multi_statement_submission() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("deny-multi"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let answerer = spawn_gate_answerer(d.kernel_db().clone(), false);
        let err = broker
            .call_tool(
                call_write("echo one\necho two"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("a denied gate must refuse the whole submission, not run any of it");
        answerer.await.unwrap();
        match err {
            McpError::Protocol(msg) => assert!(
                msg.to_lowercase().contains("denied"),
                "refusal must say WHY (denied, not merely unavailable): {msg}"
            ),
            other => panic!("expected a Protocol refusal, got {other:?}"),
        }
    }

    /// **Slice 4 spec test — required.** `kj ledger` (the SAME verb `kj cc
    /// send`'s ask is answered through) must see and answer a `shell_write`
    /// ask with no special-casing by origin: one answering surface for
    /// every gated producer.
    #[tokio::test]
    async fn kj_ledger_answers_a_shell_write_ask_like_a_cc_send_ask() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ledger-shw"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let broker2 = broker.clone();
        let gate_call = tokio::spawn(async move {
            broker2
                .call_tool(
                    call_write("echo answered-like-cc-send"),
                    &cc,
                    CancellationToken::new(),
                )
                .await
        });

        let ledger_caller = test_caller();
        let mut request_id = String::new();
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let listing = d
                .dispatch(&["ledger".to_string(), "list".to_string()], &ledger_caller)
                .await;
            if listing.message().contains("shell_gate") {
                request_id = listing
                    .message()
                    .lines()
                    .find(|l| l.contains("shell_gate"))
                    .and_then(|l| l.split_whitespace().next())
                    .expect("request id is the first column")
                    .to_string();
                break;
            }
        }
        assert!(!request_id.is_empty(), "kj ledger list must show the shell_write ask");

        let allow = d
            .dispatch(
                &["ledger".to_string(), "allow".to_string(), request_id.clone()],
                &ledger_caller,
            )
            .await;
        assert!(allow.is_ok(), "kj ledger allow must answer a shell_write ask: {allow:?}");

        let result = gate_call
            .await
            .unwrap()
            .expect("shell_write call should succeed once kj ledger allows it");
        assert!(!result.is_error);
        match result.content.first().expect("content") {
            ToolContent::Text(s) => {
                assert!(s.contains("answered-like-cc-send"), "stdout missing, got: {s:?}")
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
        // A latched destructive op (exit 2) carries its gate on kaish's opaque
        // `baggage` channel (kaish 0.14 deleted the typed `.latch` field). The
        // MCP shell envelope must surface it so a batch loop reads the gate
        // structurally instead of scraping the confirmation prose out of the
        // body. Resolves the on-hold docs/issues.md "latch nonce on stderr"
        // entry.
        let r = crate::runtime::kj_builtin::latch_result(
            "kj context remove",
            "doomed",
            "removing a context is destructive",
            "kj context remove doomed --confirm".to_string(),
        );
        let structured = shell_result_to_kernel(r)
            .structured
            .expect("structured envelope");
        assert_eq!(
            structured["latch"]["command"],
            serde_json::json!("kj context remove")
        );
        assert_eq!(structured["latch"]["target"], serde_json::json!("doomed"));
        assert_eq!(
            structured["latch"]["hint"],
            serde_json::json!("kj context remove doomed --confirm"),
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

    /// The toolie's post-flag-day loadout: `facade:shell` (and NOT
    /// `facade:shell_write`). It must see exactly the safe `shell` tool and
    /// NOT the hot `shell_write` — one shell or the other, never both, for a
    /// narrow role. `read_only_shell`/`facade:shell_readonly` are retired
    /// names as of the 2026-08-17 flag day.
    #[tokio::test]
    async fn safe_role_sees_only_the_shell_tool() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ro"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell must expose the safe shell tool: {visible:?}"
        );
        assert!(
            !visible.iter().any(|(name, _)| name == "shell_write"),
            "facade:shell must NOT expose the hot shell_write tool: {visible:?}"
        );
    }

    /// The mirror: a `facade:shell_write` (hot) role sees `shell_write` and
    /// NOT `shell`. Together with the test above, this is the "one shell or
    /// the other" invariant for the narrow roles.
    #[tokio::test]
    async fn shell_write_role_does_not_see_the_safe_shell() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("rw"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell_write"),
            "facade:shell_write must expose the hot shell tool: {visible:?}"
        );
        assert!(
            !visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell_write must NOT expose the safe shell tool: {visible:?}"
        );
    }

    /// End-to-end through `broker.call_tool`: `facade:shell` lets the model
    /// run a *read* command and get its output through the safe, unmarked
    /// tool. Refusal of writes / external commands is enforced structurally
    /// and unit-tested at the `MountBackend` / `ReadOnlyFs` layers; here we
    /// prove the gate opens for a read and the command actually runs in the
    /// read-only materialization.
    #[tokio::test]
    async fn safe_shell_runs_a_read_command() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("roexec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hello-ro"), &cc, CancellationToken::new())
            .await
            .expect("safe shell call should succeed");

        assert!(!result.is_error, "echo should not be an error: {result:?}");
        match result.content.first().expect("content") {
            ToolContent::Text(s) => {
                assert!(s.contains("hello-ro"), "stdout missing, got: {s:?}")
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// **Slice 3 spec test 1** (`docs/gate-and-shell-split.md`): a context
    /// bound to the OLD `facade:shell` grant (a stale rc script, a cached
    /// binding, a model's habit — nobody updated it for the flag day) must
    /// see the mutating tool disappear and the safe one take over under the
    /// name `shell` — capability LOSS, never a capability leak. Pins the
    /// "wrong-but-safe, never wrong-but-dangerous" direction the rename must
    /// fail in.
    #[tokio::test]
    async fn stale_facade_shell_grant_loses_write_keeps_the_name() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("stale"), None, principal);

        // The stale grant: whatever an old rc script or cached binding still
        // says, unaware anything changed.
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        // The mutating tool is gone: neither visible...
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            !visible.iter().any(|(name, _)| name == "shell_write"),
            "a stale facade:shell grant must not expose shell_write: {visible:?}"
        );
        // ...nor callable.
        let err = broker
            .call_tool(call_write("echo should-not-run"), &cc, CancellationToken::new())
            .await
            .expect_err("a stale facade:shell grant must not reach the mutating tool");
        assert!(
            matches!(err, McpError::CapabilityDenied { .. }),
            "expected CapabilityDenied, got {err:?}"
        );

        // The name "shell" still works — routed to the safe tool now.
        let result = broker
            .call_tool(call("echo still-works"), &cc, CancellationToken::new())
            .await
            .expect("the stale grant must still reach the safe tool under the name `shell`");
        assert!(!result.is_error);
        match result.content.first().expect("content") {
            ToolContent::Text(s) => assert!(s.contains("still-works"), "got: {s:?}"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// **Slice 3 spec test 2**: a context newly granted `facade:shell_write`
    /// (plus the `exec` authority — external spawning is gated on that
    /// authority independent of which facade is granted, see
    /// `kj/context_shell.rs::materialize_context_kaish_inner`) gets exactly
    /// what `builtin.shell` provided before the flag day, under the new name
    /// — proven with a real external binary (`id`), not just a kaish
    /// builtin, so the assertion actually exercises `ExternalExec::Allow`,
    /// not merely that a command ran.
    ///
    /// The `exec` grant must land on TWO brokers: `wired()`'s standalone
    /// broker (what `call_tool` gates against) AND `d.kernel().broker()`
    /// (what `materialize_context_kaish_inner`'s exec-authority check reads,
    /// via `self.kernel().broker()` — a different `Arc<Broker>` than the one
    /// `ShellServer` was registered on for the *synchronous* path; the
    /// `background: true` path checks the server's own `self.broker`
    /// instead, so background tests elsewhere in this file don't need this).
    #[tokio::test]
    async fn shell_write_grant_gets_full_external_exec_under_the_new_name() {
        let (broker, d) = wired().await;
        // Real host root so the shell's default cwd resolves to a real
        // directory and `id` can actually spawn (mirrors
        // `kj/context_shell.rs`'s `unknown_command_fails_fast_exec_granted_shell`).
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("write-exec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding.clone()).await;
        d.kernel().broker().set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let answerer = spawn_gate_answerer(d.kernel_db().clone(), true);
        let result = broker
            .call_tool(call_write("id"), &cc, CancellationToken::new())
            .await
            .expect("shell_write call should succeed");
        answerer.await.unwrap();
        assert!(!result.is_error, "`id` should run and exit 0: {result:?}");
        match result.content.first().expect("content") {
            ToolContent::Text(s) => assert!(
                s.contains("uid="),
                "`id` must have actually spawned as a real external process, got: {s:?}"
            ),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// **Slice 3 spec test 3 — the fail-safe pin.** A stale `"shell"` request
    /// must NEVER reach `ExternalExec::Allow`, full stop, regardless of what
    /// the caller intended. Same real-binary probe as the test above (`id`),
    /// same real host mount, but through the safe tool under a bare
    /// `facade:shell` grant — the output must NOT show a real spawn. `exec`
    /// is granted too (which no real safe-only role would have) to prove the
    /// refusal is structural on the tool identity, not merely a missing
    /// authority that a different rc grant could paper over.
    #[tokio::test]
    async fn stale_shell_name_never_reaches_external_exec_allow() {
        let (broker, d) = wired().await;
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("stale-exec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let result = broker
            .call_tool(call("id"), &cc, CancellationToken::new())
            .await
            .expect("the call itself succeeds structurally — the shell runs, `id` just can't spawn");
        match result.content.first().expect("content") {
            ToolContent::Text(s) => assert!(
                !s.contains("uid="),
                "a stale `shell` request must NEVER reach ExternalExec::Allow \
                 and spawn a real process, got: {s:?}"
            ),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// `background: true` requires the `exec` authority on top of
    /// `facade:shell_write` — the same gate a synchronous external command
    /// hits, never a weaker one. A context with `facade:shell_write` alone
    /// (no `exec`) must be refused, not silently degrade to a foreground run.
    #[tokio::test]
    async fn background_true_is_denied_without_exec_authority() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-noexec"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
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

    /// End-to-end: `shell_write(background: true)` with `facade:shell_write`
    /// + `exec` returns IMMEDIATELY (a handle + block id, never the command's
    /// output), and the command actually runs — its output shows up in the
    /// returned block a moment later, proving the async path is really
    /// wired, not just accepting the flag and doing nothing.
    #[tokio::test]
    async fn background_true_returns_immediately_and_streams_into_its_block() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-ok"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
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

        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");
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

    /// The safe `shell` tool must refuse `background: true` outright — it
    /// never spawns host subprocesses by construction (its materialized
    /// kaish pins `ExternalExec::Deny`), and background execution must not be
    /// a back door around that.
    #[tokio::test]
    async fn safe_shell_rejects_background_true() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ro-bg"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        // Even granting `exec` (which no real safe-shell-only role would
        // have) must not open the door — the refusal is structural on
        // `read_only`, checked before the capability gate.
        binding.grant(Capability::Exec);
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
            .expect_err("the safe shell tool must refuse background execution even with exec granted");
        assert!(matches!(err, McpError::Protocol(_)), "expected a Protocol refusal, got {err:?}");
    }

    /// CHARACTERIZATION: block lifecycle, "created up front" half. The
    /// output block's stored `Status` must already be `Running` the instant
    /// `shell(background: true)` returns — not flipped to `Running` by some
    /// later step. Distinct from the `"status": "running"` field in the tool
    /// response (that's the background JOB's status, a different value from
    /// the block's own status); this pins the block directly.
    ///
    /// Reaches into `d.block_store()` for the raw `Status` enum (the MCP tool
    /// surface never exposes it) — expected to remain the shared
    /// primitive across the background-engine swap, unlike
    /// `background_exec`'s own types.
    #[tokio::test]
    async fn background_true_creates_a_running_block_before_the_process_finishes() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-runblock"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            // Long enough that it cannot have exited by the time we check.
            arguments: serde_json::json!({"command": "sleep 2", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");
        let structured = result.structured.unwrap();
        let block_key = structured["block_id"].as_str().unwrap().to_string();
        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");
        let bg_id = crate::background_exec::BackgroundId::parse(structured["background_id"].as_str().unwrap())
            .expect("valid background id");

        let snap = d.block_store().get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(
            snap.status,
            kaijutsu_types::Status::Running,
            "the output block must be Running immediately after shell(background: true) returns"
        );

        // Clean up the still-running sleep.
        d.kernel().background_processes().cancel(bg_id, ctx_id);
    }

    /// CHARACTERIZATION: block lifecycle, nonzero-exit case. A Running block
    /// must transition to `Status::Error` (never silently `Done`, never
    /// stuck `Running`) when the backgrounded command exits nonzero, and the
    /// real exit code must survive into `list_background_processes`.
    /// Complements
    /// `background_exec::tests::spawn_background_nonzero_exit_marks_block_error_and_records_code`
    /// (same contract, pinned directly against the internal `spawn_background`
    /// API) with the MCP-tool-surface view expected to survive the engine
    /// swap. Reaches into `d.block_store()` for the stored `Status` only.
    #[tokio::test]
    async fn background_true_nonzero_exit_marks_the_block_error() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-nonzero"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "exit 5", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");
        let bg_id = result.structured.as_ref().unwrap()["background_id"].as_str().unwrap().to_string();
        let block_key = result.structured.unwrap()["block_id"].as_str().unwrap().to_string();
        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");

        let registry = d.kernel().background_processes();
        let parsed_id = crate::background_exec::BackgroundId::parse(&bg_id).unwrap();
        let start = std::time::Instant::now();
        let snap = loop {
            if let Some(s) = registry.get_for_context(parsed_id, ctx_id).filter(|s| s.status == "exited") {
                break s;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for exit");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(snap.exit_code, Some(5), "exit status must never be lost");

        let block_snap = d.block_store().get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(
            block_snap.status,
            kaijutsu_types::Status::Error,
            "a nonzero-exit background process must leave the block Error, not Done"
        );
        assert_eq!(block_snap.exit_code, Some(5));
    }

    /// CHARACTERIZATION: refusal guard. `background: true` must refuse a
    /// persisted context cwd that isn't a REAL host directory — a background
    /// spawn goes straight to the host (bypassing kaish's VFS), so a
    /// virtual-only cwd like `/v/docs` can't be honored. Assert both the
    /// refusal AND that it names the offending cwd, per Amy's "assert on the
    /// behavior and the reason, not exact prose" standard.
    #[tokio::test]
    async fn background_true_refuses_a_non_host_real_cwd() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-badcwd"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: ctx_id,
                cwd: Some("/v/docs".to_string()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "echo nope", "background": true}),
        };
        let err = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect_err("a non-host-real cwd must refuse background execution");
        match err {
            McpError::Protocol(msg) => {
                assert!(msg.contains("/v/docs"), "refusal should name the offending cwd: {msg}");
                assert!(
                    msg.to_lowercase().contains("cwd") || msg.to_lowercase().contains("host"),
                    "refusal should explain it's a host-realness problem, not a generic error: {msg}"
                );
            }
            other => panic!("expected a Protocol refusal, got {other:?}"),
        }
    }

    /// CHARACTERIZATION: hermetic env. `background_exec.rs` docs promise the
    /// child's environment is the caller's EXPLICIT set (HOME, PATH from
    /// `Kernel::host_path`, and the context's durable env vars) — never this
    /// kernel process's own ambient OS environment. Proven two ways at once:
    /// a var real in this test process's OS env but never threaded through
    /// `start_background` must NOT reach the child, while a context-scoped
    /// env var explicitly set via `kernel_db::set_context_env` — which IS
    /// part of the documented hermetic set — must.
    #[tokio::test]
    async fn background_true_env_is_hermetic_not_inherited() {
        let leak_key = "KAIJUTSU_TEST_BG_ENV_LEAK_MARKER";
        // SAFETY: unique var name avoids cross-test collisions; this crate's
        // test suite already accepts this pattern (see llm/config.rs).
        unsafe {
            std::env::set_var(leak_key, "should-not-leak-into-the-child");
        }

        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-env"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        {
            let db = d.kernel_db().lock();
            db.set_context_env(ctx_id, "KJ_CONTEXT_VAR", "context-value").unwrap();
        }

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "env", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");
        let block_key = result.structured.unwrap()["block_id"].as_str().unwrap().to_string();
        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");

        let start = std::time::Instant::now();
        let content = loop {
            let snap = d.block_store().get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
            if snap.status != kaijutsu_types::Status::Running {
                break snap.content;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for `env` to finish");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        // SAFETY: matches the set_var above.
        unsafe {
            std::env::remove_var(leak_key);
        }

        assert!(
            !content.contains(leak_key),
            "the child must not see this kernel process's own OS env, got: {content:?}"
        );
        assert!(
            content.contains("KJ_CONTEXT_VAR=context-value"),
            "the context's durable env var must reach the child, got: {content:?}"
        );
        assert!(
            content.contains(&format!("HOME={}", kaish_kernel::home_dir().to_string_lossy())),
            "HOME must be seeded from kaish_kernel::home_dir(), got: {content:?}"
        );
        if let Some(path) = d.kernel().host_path() {
            assert!(
                content.contains(&format!("PATH={path}")),
                "PATH must be the kernel's startup capture, got: {content:?}"
            );
        }
    }
}
