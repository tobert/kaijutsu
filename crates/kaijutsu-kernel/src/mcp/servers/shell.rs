//! Broker tools for contextual shell execution.
//!
//! `shell` selects structural read-only execution. `shell_write` permits
//! mutation, with host execution separately controlled by the context's Exec
//! capability. Each call uses `EmbeddedKaish::for_context`, shared with RPC,
//! rc, hooks, and editor commands.
//!
//! The instances are facade projections: `facade:shell` and
//! `facade:shell_write` govern visibility and dispatch without a second grant.
//! Rc selects each context type's loadout; see `docs/gate-and-shell-split.md`.

#[cfg(test)]
use crate::runtime::command_result::shell_result_to_envelope;
use crate::runtime::command_result::shell_envelope_to_tool_result as envelope_result;
use crate::runtime::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
use crate::runtime::embedded_kaish::EmbeddedKaish;
use std::sync::{Arc, LazyLock, Weak};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use kaijutsu_types::RefusalKind;
use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
#[cfg(test)]
use super::super::types::ToolContent;

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
    /// Wait for command completion and result hooks. Defaults to `false`.
    /// A result review returns a pending refusal; inspect its captured and
    /// final results with `kj ledger show` after the reviewer answers.
    ///
    /// An asynchronous command runs the same complete kaish program as a
    /// foreground command, with the same context identity, tools, mounts,
    /// variables, working directory, and external-command policy. It returns
    /// a stable operation receipt without waiting for completion; read, wait,
    /// or cancel it through the shell operation API.
    #[serde(default)]
    pub foreground: bool,
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
// context/drift/fork management, and the return contract (one JSON envelope,
// every key always present). Kept as an intro paragraph, separated from the
// composed kaish-language rules by a blank line, so the two sources stay
// visibly distinct rather than blurring into one hand-tuned paragraph the way
// the old static file did.
//
// RETURN_CONTRACT is the one statement of the envelope, shared by both
// flavours — two copies of a shape description drift, and this one is read by
// every model that calls the tool.
const RETURN_CONTRACT: &str = "Returns one JSON object, always the same \
     keys: {stdout, stderr, exit_code, status, did_spill, data, latch, \
     block_id, operation_id, ask_id, content_type, ephemeral, elapsed_ms, error}. \
     `stdout` and `stderr` are separate and are empty strings when the \
     command wrote none. Read `status` to distinguish completion, failure, \
     and pending work. `status` is done, error, rejected, running, waiting, timeout or \
     stream_closed — `rejected` means kaish refused the program and nothing \
     ran, so fix the command text and retry. `exit_code` is null exactly \
     when there is no code to report; null is never evidence of success. \
     `did_spill` true means output was capped and the tail dropped. `data` \
     is the kj structured payload when present.";

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a command in your current kernel context using kaish (会sh). \
         `kj` is in scope for context/drift/fork management. {}\n\n{}",
        RETURN_CONTRACT, &*COMPOSED_TOOL_DESCRIPTION
    )
});

// The model sees the policy and the writable alternative alongside kaish syntax.
static DESCRIPTION_READ_ONLY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a READ-ONLY command in your current kernel context using kaish \
         (会sh). Submitted commands cannot mutate shared state. File writes, \
         external commands, mutating `kj` verbs, editor input, `curl`, and MCP \
         calls are refused. Use `shell_write` for those operations. Host tools \
         may still be installed and on PATH. Inspect with filesystem builtins \
         (`cat`, `grep`, `find`), `/v/docs`, and read-only `kj` commands. \
         Help and local shell variables remain available. {}\n\n{}",
        RETURN_CONTRACT, &*COMPOSED_TOOL_DESCRIPTION
    )
});

/// Broker server for `shell` or `shell_write`, selected at construction.
/// A weak broker reference avoids a cycle; each call constructs its own shell.
pub struct ShellServer {
    instance_id: InstanceId,
    /// The model-facing tool name: `shell` or `shell_write`.
    tool: &'static str,
    /// When true, materialize a read-only context kaish (no writes, no external
    /// commands; reads — incl. document views — still work).
    read_only: bool,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl ShellServer {
    /// Read-only execution; host subprocesses are disabled.
    pub const INSTANCE: &'static str = "builtin.shell";
    pub const TOOL: &'static str = "shell";
    /// Writable execution; the context's Exec capability controls subprocesses.
    pub const INSTANCE_WRITE: &'static str = "builtin.shell_write";
    pub const TOOL_WRITE: &'static str = "shell_write";

    /// The writable `shell_write` tool (gated by `facade:shell_write`).
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

}

#[async_trait]
impl McpServerLike for ShellServer {
    fn result_hook_owner(&self) -> super::super::server_like::ResultHookOwner {
        super::super::server_like::ResultHookOwner::Execution
    }

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
        cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool != self.tool {
            return Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: params.tool,
            });
        }
        let parsed: ShellParams =
            serde_json::from_value(params.arguments.clone()).map_err(McpError::InvalidParams)?;

        // The dispatcher supplies the same context policy, index, and block
        // source used by other runtime callers.
        let broker = self.broker()?;
        let dispatcher = broker
            .kj_dispatcher()
            .await
            .ok_or_else(|| McpError::InstanceDown {
                instance: self.instance_id.clone(),
                reason: "kj dispatcher not wired (Broker::set_kj_dispatcher)".to_string(),
            })?;

        let admission = dispatcher.kernel().admit_context(ctx.context_id).map_err(McpError::Protocol)?;

        // Writable submissions pass the source-program gate. Read-only shells
        // enforce their policy structurally and do not need execution approval.
        // An approved writable submission carries the directory it authorized.
        let mut cwd_source = ShellCwd::Context;
        if !self.read_only {
            // A submission that does not parse is refused here, before the
            // gate — a human is never asked to approve text that cannot be
            // rendered. `ShellGateBuildError` has exactly one variant and it
            // is `Parse`, so this is always the model's mistake to fix and
            // never a fault: it takes the same D-28 `is_error` channel a
            // post-gate rejection takes, not `McpError::Protocol`.
            let mut spec = match crate::kj::shell_gate::build_shell_gate_spec_with_stdin(&parsed.command, parsed.stdin.clone()) {
                Ok(spec) => spec,
                Err(e) => {
                    let mut env = ShellEnvelope::new(ShellStatus::Rejected);
                    env.error = Some(format!("{e} — nothing was run"));
                    return Ok(envelope_result(env));
                }
            };
            spec.publishes_pair = ctx.publishes_pair || !parsed.foreground;
            let caller = crate::kj::KjCaller {
                principal_id: ctx.principal_id,
                actor_id: ctx.actor_id,
                reviewer_id: ctx.reviewer_id,
                context_id: Some(ctx.context_id),
                session_id: ctx.session_id,
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            // The gate records a durable ask without waiting. The approval
            // driver or a matching retry consumes its answer once.
            let gate_config =
                crate::kj::gate_policy::load_config(dispatcher.kernel().vfs()).await;
            let outcome = crate::kj::gate::run_gate(
                dispatcher.kernel(),
                &caller,
                spec,
                dispatcher.kernel().ledger_flows(),
                &gate_config,
            )
            .await;
            if !outcome.allowed() {
                // Every non-allowed outcome fails closed, and each carries
                // its own kind: a pending ask is waiting, a ledger fault is
                // broken, and a denial is somebody's decision. This gate has
                // no hook behind it, so the tool it guards is the subject.
                let kind = match outcome.verdict {
                    crate::kj::gate::GateVerdict::Pending => RefusalKind::Pending,
                    crate::kj::gate::GateVerdict::Unavailable => RefusalKind::GateUnavailable,
                    crate::kj::gate::GateVerdict::Denied => RefusalKind::Denied,
                    // Guarded by `!outcome.allowed()` directly above. Reaching
                    // here means `allowed()` and this match disagree about
                    // which verdict lets a call through.
                    crate::kj::gate::GateVerdict::Allowed => unreachable!(
                        "an allowed gate outcome reached the refusal path"
                    ),
                };
                if kind == RefusalKind::Pending && !parsed.foreground {
                    let ask = outcome.ask.as_ref().expect("a pending gate outcome has an ask");
                    crate::runtime::tool_command::create_operation(
                        dispatcher.kernel(), ctx, &parsed.command, Some(&ask.request_id),
                    ).map_err(McpError::Protocol)?;
                }
                if kind == RefusalKind::Pending && !parsed.foreground {
                    let receipt = dispatcher.kernel().shell_operations().get_by_ask(
                        &outcome.ask.as_ref().expect("pending gate outcome has an ask").request_id,
                        ctx.context_id,
                    ).map_err(|error| McpError::Protocol(format!(
                        "load waiting shell operation: {error}"
                    )))?.expect("pending shell gate registered its operation");
                    let mut env = ShellEnvelope::new(ShellStatus::Waiting);
                    env.operation_id = Some(receipt.receipt.operation_id);
                    env.ask_id = outcome.ask.as_ref().map(|ask| ask.request_id.clone());
                    env.block_id = Some(receipt.receipt.output_block_id.to_key());
                    env.stderr = outcome.reason.clone();
                    return Ok(envelope_result(env));
                }
                return Err(McpError::refused_gate(kind, self.tool, outcome.ask.clone(), &outcome.reason));
            }
            // Preserve the directory attached to the approval; current context
            // state may have changed while its reviewer was deciding.
            cwd_source = outcome.cwd;
        }

        let semantic_index = dispatcher.semantic_index();
        let block_source = dispatcher.block_source();
        let kaish = EmbeddedKaish::for_context(
            &dispatcher,
            if self.read_only { "model-shell-ro" } else { "model-shell" },
            ShellIdentity {
                requester: ctx.principal_id, performer: ctx.actor_id, reviewer: ctx.reviewer_id,
                context: ctx.context_id, session: ctx.session_id,
            },
            if self.read_only { ShellPolicy::ReadOnly } else { ShellPolicy::Agent }, cwd_source,
            semantic_index,
            block_source,
        )
        .await
        .map_err(|e| McpError::Protocol(format!("materialize context shell: {e}")))?;

        crate::runtime::tool_command::ToolCommand {
            admission,
            kernel: dispatcher.kernel().clone(), broker, kaish, params, call: ctx.clone(),
            code: parsed.command, stdin: parsed.stdin, foreground: parsed.foreground, read_only: self.read_only,
        }.execute(cancel).await
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::ContextShellRow;
    use crate::kj::test_helpers::{register_context, test_caller, test_dispatcher_persistent};
    use crate::mcp::binding::{Capability, ContextToolBinding};
    use crate::mcp::{InstancePolicy, KernelCallParams};
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};

    #[tokio::test]
    async fn emitted_shell_tools_do_not_advertise_compose_draft() {
        let ctx = CallContext::new(
            PrincipalId::new(), ContextId::new(), SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        for server in [ShellServer::new(Weak::new()), ShellServer::new_read_only(Weak::new())] {
            let tools = server.list_tools(&ctx).await.unwrap();
            assert_eq!(tools.len(), 1);
            let tool = &tools[0];
            let description = tool.description.as_deref().unwrap();
            assert!(!description.contains("/v/input"), "{}: {description}", tool.name);
            assert!(!tool.input_schema.to_string().contains("/v/input"));
            println!("{} description: {description}\nschema: {}", tool.name, tool.input_schema);
        }
    }

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
            text.contains("one JSON object, always the same keys")
                && text.contains("Read `status`"),
            "return contract must survive: {text}"
        );

        let ro_text = DESCRIPTION_READ_ONLY.as_str();
        assert!(
            ro_text.contains("cannot mutate shared state"),
            "read-only contract must survive: {ro_text}"
        );
        assert!(
            ro_text.contains("/v/docs"),
            "read-only document views must survive: {ro_text}"
        );
        assert!(
            ro_text.contains("one JSON object, always the same keys")
                && ro_text.contains("Read `status`"),
            "return contract must survive on the read-only variant too: {ro_text}"
        );
        // Execution refusal does not imply a host binary is absent.
        assert!(
            ro_text.contains("`shell_write`") && ro_text.contains("on PATH"),
            "the read-only description must name `shell_write` as where \
             external commands run, and say the binary is still installed: {ro_text}"
        );
    }

    /// Prints the read-only tool description as the model receives it —
    /// `cargo test -p kaijutsu-kernel read_only_description -- --nocapture`.
    /// Published text is read, not grepped: the asserts above pin phrases,
    /// this shows the whole thing.
    #[test]
    fn read_only_description_as_the_model_reads_it() {
        println!("\n--- shell (read-only) description ---\n{}\n--- end ---", DESCRIPTION_READ_ONLY.as_str());
    }

    /// An `Arc<KjDispatcher>` wired into a fresh broker with BOTH the writable
    /// and read-only `ShellServer`s registered — the runtime shape
    /// (`set_self_arc` + `set_kj_dispatcher`), so facade gating across the two
    /// can be exercised together.
    async fn wired() -> (Arc<Broker>, Arc<crate::kj::KjDispatcher>) {
        let d = Arc::new(test_dispatcher_persistent().await);
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
            arguments: serde_json::json!({ "command": command, "foreground": true }),
        }
    }

    /// Params targeting the HOT, mutating `shell_write` tool
    /// (`ExternalExec::Allow`) — granted not default.
    fn call_write(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({ "command": command, "foreground": true }),
        }
    }

    fn call_async(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    fn call_write_async(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// `shell_write` is gated (`docs/gate-and-shell-split.md`, "Slice 4"),
    /// and `run_gate` never waits (`docs/gate-resume.md`): a test that calls
    /// it synchronously gets `Pending`/`GatePending` back immediately, with
    /// nothing run, and a durable ask already sitting in the ledger. Answer
    /// that ask directly — no poll loop, no spawned task — the way a human
    /// running `kj ledger allow` would from another shell, then retry the
    /// same call to redeem it.
    fn answer_pending_ask(
        db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
        allow: bool,
    ) -> String {
        let db = db.lock();
        let conn = db.conn_for_ledger();
        let row = approval_ledger::ask::list_pending(conn)
            .unwrap()
            .into_iter()
            .next()
            .expect("the gate must have left exactly one pending ask");
        let reviewer = row.reviewer_id.as_deref().expect("test ask has a reviewer");
        approval_ledger::claim::claim(conn, &row.request_id, reviewer).unwrap();
        approval_ledger::decide::decide(
            conn,
            &row.request_id,
            approval_ledger::decide::DecideInput {
                allow,
                decided_by: Some(approval_ledger::decide::Answerer {
                    principal: reviewer,
                    context: Some(b"another-seat"),
                }),
                decided_option: Some(if allow { "allow_once" } else { "deny" }),
                remember_scope: None,
                auto_reason: None,
            },
        )
        .unwrap();
        row.request_id
    }

    /// End-to-end through `broker.call_tool`: `facade:shell` alone (no `*`, no
    /// instance grant) must let the model run a command through the SAFE tool.
    /// Post-2026-08-17-flag-day, `facade:shell` is the unmarked/safe grant —
    /// this is the tool a caller reaches for by default.
    #[tokio::test]
    async fn facade_shell_runs_a_command_through_the_broker() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = register_context(&d, Some("sh"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));
        let result = broker
            .call_tool(call("echo hello-shell"), &cc, CancellationToken::new())
            .await
            .expect("shell call should succeed");

        assert!(!result.is_error, "echo should not be an error");
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("hello-shell"), "stdout missing, got: {out:?}");
    }

    /// The mirror on the hot side: `facade:shell_write` alone must let the
    /// model run a command through the mutating tool, under its new name.
    /// Director explicitly holds both facades post-flag-day (the operator's
    /// console wants the safe tool by default and the hot one available).
    #[tokio::test]
    async fn facade_shell_write_runs_a_command_through_the_broker() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = crate::kj::test_helpers::register_rooted_context(&d, Some("shw"), principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));
        let pending = broker
            .call_tool(call_write("echo hello-shell-write"), &cc, CancellationToken::new())
            .await
            .expect_err("an uncovered shell_write submission must escalate and return \
                         immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "an open ask is a pending verdict, not a fault: {pending:?}"
        );
        assert!(
            pending.as_refusal().and_then(|r| r.ask_id().map(str::to_owned)).is_some(),
            "the caller gets the ask id as a handle, not as prose: {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), true);

        let result = broker
            .call_tool(call_write("echo hello-shell-write"), &cc, CancellationToken::new())
            .await
            .expect("shell_write call should succeed once the pending ask is answered");

        assert!(!result.is_error, "echo should not be an error");
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("hello-shell-write"), "stdout missing, got: {out:?}");
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

    /// **Slice 1 spec test.** An uncovered `shell_write` submission escalates
    /// and returns IMMEDIATELY, refusing — proven through the real
    /// `ShellServer` → `build_shell_gate_spec` → `run_gate` wiring, not just
    /// `run_gate` in isolation (`kj::gate`'s own
    /// `an_uncovered_multi_statement_submission_escalates` covers the
    /// underlying mechanism). Nothing here times out (`docs/gate-resume.md`
    /// — `run_gate` never waits): a rename of the old
    /// `shell_write_with_no_answer_escalates_and_times_out_refusing_the_call`,
    /// which pinned a wait that no longer exists.
    /// The POST-gate half of the same rule: a program that parses, gets
    /// approved, and is then refused by kaish's validator.
    ///
    /// The pre-gate test below cannot reach this branch — an unparseable
    /// program never builds a gate spec, so it never gets as far as
    /// `execute_with_options`. `break` outside a loop is the opposite shape:
    /// it parses cleanly (a human is shown it and approves it) and the
    /// validator rejects it before any statement runs. That is the case
    /// `KernelError::is_rejected()` exists to separate from a real fault.
    #[tokio::test]
    async fn a_validator_rejection_after_approval_is_also_a_tool_failure() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = crate::kj::test_helpers::register_rooted_context(&d, Some("validrej"), principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));
        let bad = "break";

        let _ = broker
            .call_tool(call_write(bad), &cc, CancellationToken::new())
            .await;
        answer_pending_ask(d.kernel_db().clone(), true);

        let result = broker
            .call_tool(call_write(bad), &cc, CancellationToken::new())
            .await
            .expect("a validator rejection must come back as a result, not an Err");

        assert!(result.is_error, "a rejected program must set is_error");
        let envelope = body_of(&result);
        assert_eq!(
            envelope["status"],
            serde_json::json!("rejected"),
            "a refused program is `rejected`, distinct from a command that ran and failed"
        );
        let body = envelope["error"]
            .as_str()
            .expect("a rejection names why in `error`")
            .to_string();
        assert!(
            !body.contains("mcp protocol error"),
            "broker-internal vocabulary must not reach the model; got: {body}"
        );
    }

    /// A program kaish REFUSES reaches the model as a tool failure it can
    /// read, not as a broker protocol fault.
    ///
    /// Before the 0.16 error split, every kaish failure — parse, validation,
    /// genuine IO fault alike — was wrapped in `McpError::Protocol`, whose
    /// Display prepends `mcp protocol error:`. That is broker-internal
    /// vocabulary its own doc says must be converted at the LLM boundary, and
    /// it made "your command was rejected, fix it and retry" read like
    /// kaijutsu was broken. This pins the two halves that matter: `is_error`
    /// is set, and the internal prefix is absent.
    #[tokio::test]
    async fn a_rejected_program_is_a_tool_failure_not_a_protocol_fault() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("reject"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        // An unterminated quote. This is refused BEFORE the gate — a human
        // is never asked to approve text that cannot be rendered — so there
        // is no ask to answer, and the refusal must still not wear protocol
        // clothes.
        let result = broker
            .call_tool(call_write("echo 'unclosed"), &cc, CancellationToken::new())
            .await
            .expect("a refused program must come back as a result, not an Err");

        assert!(result.is_error, "a rejected program must set is_error");
        let envelope = body_of(&result);
        assert_eq!(
            envelope["status"],
            serde_json::json!("rejected"),
            "a refused program is `rejected`, distinct from a command that ran and failed"
        );
        let body = envelope["error"]
            .as_str()
            .expect("a rejection names why in `error`")
            .to_string();
        assert!(
            !body.contains("mcp protocol error"),
            "broker-internal vocabulary must not reach the model; got: {body}"
        );
        assert!(
            !body.is_empty(),
            "kaish's own rejection text must survive; got an empty body"
        );
    }

    #[tokio::test]
    async fn shell_write_with_no_answer_escalates_and_returns_pending_refusing_the_call() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = crate::kj::test_helpers::register_rooted_context(&d, Some("pending-shw"), principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let err = broker
            .call_tool(call_write("echo nobody-answers"), &cc, CancellationToken::new())
            .await
            .expect_err("an unanswered gate must refuse immediately, never hang or silently run");
        assert!(
            err.is_refusal(RefusalKind::Pending),
            "an unanswered gate is waiting, distinguishably from a hard denial: {err:?}"
        );
    }

    /// **Slice 1 spec test.** A multi-statement `shell_write` submission a
    /// human denies refuses the WHOLE call — the gate runs entirely before
    /// `execute_with_options` is ever called for this submission, so there
    /// is no partial-execution window to prove separately; this asserts the
    /// refusal itself reaches the caller through the real MCP wiring. The
    /// RULE-covered "one denied statement denies the whole ask" composition
    /// is unit-tested directly against `run_gate` in `kj::gate`
    /// (`a_denied_statement_among_several_refuses_the_whole_submission_and_names_it`).
    /// First call escalates and returns immediately (nothing run); the
    /// retry after a human denies is the one that carries the verdict.
    #[tokio::test]
    async fn shell_write_deny_refuses_the_whole_multi_statement_submission() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = crate::kj::test_helpers::register_rooted_context(&d, Some("deny-multi"), principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));

        let pending = broker
            .call_tool(
                call_write("echo one\necho two"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "expected a pending verdict, got {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), false);

        let err = broker
            .call_tool(
                call_write("echo one\necho two"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("a denied gate must refuse the whole submission, not run any of it");
        assert!(
            err.is_refusal(RefusalKind::Denied),
            "a human's no is a denial, never merely unavailable: {err:?}"
        );
    }

    /// **Slice 1 spec test — required.** `kj ledger` (the SAME verb `kj cc
    /// send`'s ask is answered through) must see and answer a `shell_write`
    /// ask with no special-casing by origin: one answering surface for
    /// every gated producer. No concurrency needed any more: the first call
    /// escalates and returns with the ask already durable, `kj ledger`
    /// answers it directly, and the retry redeems the answer.
    #[tokio::test]
    async fn kj_ledger_answers_a_shell_write_ask_like_a_cc_send_ask() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ledger-shw"), None, principal);
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: reviewer, name: "reviewer".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        d.kernel_db().lock().update_context_review(ctx_id, Some(principal), Some(reviewer)).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));

        let pending = broker
            .call_tool(
                call_write("echo answered-like-cc-send"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        // The id comes back on the refusal itself. Recovering it by
        // splitting a `kj ledger list` line — which this test used to do —
        // is the symptom the refusal shape exists to remove.
        let request_id = pending
            .as_refusal()
            .and_then(|r| r.ask_id().map(str::to_owned))
            .expect("a pending gate hands the caller its ask id");

        let ledger_caller = test_caller().with_actor(reviewer, None);
        let listing = d
            .dispatch(&["ledger".to_string(), "list".to_string()], &ledger_caller)
            .await;
        assert!(
            listing.message().contains("shell_gate"),
            "kj ledger list must show the shell_write ask: {}",
            listing.message()
        );
        assert!(
            listing.message().contains(&request_id),
            "the id the caller was handed must be the id the ledger lists: {}",
            listing.message()
        );

        let allow = d
            .dispatch(
                &["ledger".to_string(), "allow".to_string(), request_id.clone()],
                &ledger_caller,
            )
            .await;
        assert!(allow.is_ok(), "kj ledger allow must answer a shell_write ask: {allow:?}");

        let result = broker
            .call_tool(
                call_write("echo answered-like-cc-send"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("shell_write call should succeed once kj ledger allows it");
        assert!(!result.is_error);
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("answered-like-cc-send"), "stdout missing, got: {out:?}");
    }

    #[tokio::test]
    async fn auto_allowed_shell_write_uses_the_current_context_cwd() {
        use crate::vfs::VfsOps;
        let (broker, d) = wired().await;
        d.kernel().vfs().write_all(std::path::Path::new("/config/kernel/gate.toml"),
            b"[global]\nallow = [\"pwd\"]\n").await.unwrap();
        d.kernel().mount("/working", crate::vfs::backends::MemoryBackend::new()).await;
        let principal = PrincipalId::new();
        let context = crate::kj::test_helpers::register_rooted_context(&d, Some("auto-cwd"), principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        d.kernel_db().lock().upsert_context_shell(&ContextShellRow {
            context_id: context, cwd: Some("/working".into()), updated_at: 0,
        }).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(context, binding).await.unwrap();
        let call = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let result = broker.call_tool(call_write("pwd"), &call, CancellationToken::new()).await.unwrap();
        assert!(!result.is_error, "{result:?}");
        assert_eq!(body_of(&result)["stdout"].as_str().unwrap().trim(), "/working");
        d.kernel().shutdown_runtime_worker().await.unwrap();
    }

    /// **The unresolvable-pin refusal.** The context's cwd at ask time is a
    /// directory that does not exist. The ask escalates and is approved
    /// exactly as normal, but redemption must refuse LOUDLY rather than
    /// fall back to running in the context's current cwd — an approval
    /// authorizes the operation it was asked about, and a pin that no
    /// longer resolves means that operation cannot honestly be run at all.
    ///
    /// Falsified by deleting the `kaish.try_set_cwd` check in `call_tool`
    /// (letting `opts.with_cwd` carry the dead pin straight into
    /// `execute_with_options` unchecked): the call succeeded instead of
    /// refusing — `kaish` accepted the nonexistent cwd silently and ran
    /// `echo` anyway, which is precisely the silent-fallback failure this
    /// refusal exists to prevent. Reverted.
    #[tokio::test]
    async fn a_pin_that_no_longer_resolves_refuses_loudly_not_a_fallback() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = crate::kj::test_helpers::register_rooted_context(&d, Some("dead-pin"), principal);
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&ContextShellRow {
                context_id: ctx_id,
                cwd: Some("/this/directory/does/not/exist/kaijutsu-gate-pin-test".to_string()),
                updated_at: 0,
            })
            .unwrap();
        }
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await.unwrap();
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));

        let pending = broker
            .call_tool(call_write("echo should-not-run"), &cc, CancellationToken::new())
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "an uncovered submission escalates to an open ask: {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), true);

        let err = broker
            .call_tool(call_write("echo should-not-run"), &cc, CancellationToken::new())
            .await
            .expect_err("a pin that no longer resolves must refuse, never fall back to the \
                         context's current cwd");
        match err {
            McpError::Protocol(msg) => {
                assert!(
                    msg.contains("no longer resolves"),
                    "refusal must name the unresolvable-pin condition: {msg}"
                );
                assert!(
                    msg.contains("nothing was run"),
                    "refusal must say nothing ran: {msg}"
                );
            }
            other => panic!("expected a Protocol refusal, got {other:?}"),
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
        broker.set_binding(ctx_id, binding).await.unwrap();

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
        broker.set_binding(ctx_id, binding).await.unwrap();

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
        broker.set_binding(ctx_id, binding).await.unwrap();

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
        broker.set_binding(with, b).await.unwrap();
        let cc = CallContext::new(principal, with, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(with, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell context should see the shell tool: {visible:?}"
        );

        let without = register_context(&d, Some("without"), None, principal);
        broker.set_binding(without, ContextToolBinding::new()).await.unwrap();
        let cc2 = CallContext::new(principal, without, SessionId::new(), d.kernel_id());
        let visible2 = broker.list_visible_tools(without, &cc2).await.unwrap();
        assert!(
            !visible2.iter().any(|(name, _)| name == "shell"),
            "no-facade context must not see the shell tool: {visible2:?}"
        );
    }

    /// Helper: the envelope a completed `ExecResult` produces.
    fn envelope_of(r: kaish_kernel::interpreter::ExecResult) -> serde_json::Value {
        shell_result_to_envelope(r, 0).to_value()
    }

    /// Helper: the envelope a `shell` call returned. Every return is one
    /// JSON object, so a test that used to match on a text body reads fields
    /// here instead.
    fn body_of(result: &KernelToolResult) -> serde_json::Value {
        match result.content.as_slice() {
            [ToolContent::Json(v)] => v.clone(),
            other => panic!("a shell result must be one JSON value, got {other:?}"),
        }
    }

    /// Helper: everything the command wrote, both streams. For assertions
    /// about whether some text appeared at all.
    fn streams_of(result: &KernelToolResult) -> String {
        let body = body_of(result);
        format!(
            "{}{}",
            body["stdout"].as_str().unwrap_or_default(),
            body["stderr"].as_str().unwrap_or_default()
        )
    }

    /// Every `shell` return is one JSON object with every key present. This
    /// is the fix for the shape flip a worknote reported: the body used to be
    /// prose, and a command with no output fell through to a pretty-printed
    /// envelope while every other command produced text.
    #[test]
    fn every_return_is_one_json_object_with_every_key() {
        let cases = vec![
            (
                "silent success",
                envelope_result(shell_result_to_envelope(
                    kaish_kernel::interpreter::ExecResult::success(""),
                    0,
                )),
            ),
            (
                "output",
                envelope_result(shell_result_to_envelope(
                    kaish_kernel::interpreter::ExecResult::success("hi"),
                    0,
                )),
            ),
            (
                "failure",
                envelope_result(shell_result_to_envelope(
                    kaish_kernel::interpreter::ExecResult::failure(1, "boom"),
                    0,
                )),
            ),
            ("rejection", {
                let mut env = ShellEnvelope::new(ShellStatus::Rejected);
                env.error = Some("parse error".into());
                envelope_result(env)
            }),
            ("operation", {
                let mut env = ShellEnvelope::new(ShellStatus::Running);
                env.operation_id = Some("operation-1".into());
                envelope_result(env)
            }),
        ];
        for (label, kr) in cases {
            let body = match kr.content.as_slice() {
                [ToolContent::Json(v)] => v.clone(),
                other => panic!("{label}: body must be one JSON value, got {other:?}"),
            };
            let obj = body
                .as_object()
                .unwrap_or_else(|| panic!("{label}: body must be an object, got {body}"));
            for key in ShellEnvelope::KEYS {
                assert!(obj.contains_key(*key), "{label}: key {key} missing");
            }
            assert_eq!(
                kr.structured.as_ref().expect("structured present"),
                &body,
                "{label}: structured and the model-facing body are one value"
            );
        }
    }

    /// A silent success and a refused program both have empty stdout. Before
    /// the shared envelope they were told apart only by whether the body
    /// happened to be empty, which is what made the shape flip.
    #[test]
    fn silent_success_and_rejection_are_told_apart_by_status() {
        let quiet = envelope_of(kaish_kernel::interpreter::ExecResult::success(""));
        assert_eq!(quiet["stdout"], serde_json::json!(""));
        assert_eq!(quiet["status"], serde_json::json!("done"));
        assert_eq!(quiet["exit_code"], serde_json::json!(0));
        assert_eq!(quiet["error"], serde_json::Value::Null);

        let mut env = ShellEnvelope::new(ShellStatus::Rejected);
        env.error = Some("parse error at 1:6".into());
        let refused = envelope_result(env);
        assert!(refused.is_error, "a refused program is an error");
        let body = refused.structured.unwrap();
        assert_eq!(body["stdout"], serde_json::json!(""));
        assert_eq!(body["status"], serde_json::json!("rejected"));
        assert_eq!(
            body["exit_code"],
            serde_json::Value::Null,
            "nothing ran, so there is no exit code to report"
        );
        assert!(body["error"].as_str().unwrap().contains("parse error"));
    }

    #[test]
    fn conversion_success_with_warnings_keeps_exit_zero_and_surfaces_stderr() {
        let mut r = kaish_kernel::interpreter::ExecResult::success("the-output");
        r.err = "a-warning".to_string();
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(!kr.is_error, "exit 0 stays non-error even with stderr");
        let body = kr.structured.unwrap();
        assert_eq!(body["stdout"], serde_json::json!("the-output"));
        assert_eq!(
            body["stderr"],
            serde_json::json!("a-warning"),
            "stderr is its own field, never folded into stdout"
        );
    }

    #[test]
    fn conversion_surfaces_latch_request_structurally() {
        // A latched destructive op (exit 2) carries its gate on kaish's opaque
        // `baggage` channel (kaish 0.14 deleted the typed `.latch` field). The
        // envelope must surface it so a batch loop reads the gate structurally
        // instead of scraping the confirmation prose out of stdout. This test
        // is the regression guard for that.
        let r = crate::runtime::kj_builtin::latch_result(
            "kj context archive",
            "doomed",
            "removing a context is destructive",
            "kj context archive doomed --confirm".to_string(),
        );
        let structured = envelope_of(r);
        assert_eq!(
            structured["latch"]["command"],
            serde_json::json!("kj context archive")
        );
        assert_eq!(structured["latch"]["target"], serde_json::json!("doomed"));
        assert_eq!(
            structured["latch"]["hint"],
            serde_json::json!("kj context archive doomed --confirm"),
            "the ready-to-run confirmation command must ride the envelope"
        );

        // A non-latched result carries an explicit null — present as a key so a
        // consumer can test `latch == null` rather than guess at omission.
        let plain = envelope_of(kaish_kernel::interpreter::ExecResult::success("ok"));
        assert_eq!(
            plain["latch"],
            serde_json::Value::Null,
            "a non-latched result leaves `latch` explicitly null"
        );
    }

    #[test]
    fn conversion_nonzero_exit_is_error_and_status_agrees() {
        let r = kaish_kernel::interpreter::ExecResult::failure(3, "boom");
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(kr.is_error, "nonzero exit must be an error");
        let body = kr.structured.unwrap();
        assert_eq!(body["exit_code"], serde_json::json!(3));
        assert_eq!(
            body["status"],
            serde_json::json!("error"),
            "the flag and the status field must never disagree"
        );
    }

    #[test]
    fn conversion_spilled_success_is_not_error_but_signals_truncation() {
        // kaish remaps a capped/spilled result to exit 3, stashing the real
        // exit in `original_code`. Truncation is not failure: flagging it an
        // error tempts a model into re-running a command that succeeded.
        // The truncation must still be unmissable — a model reasoning over a
        // head+tail excerpt as if it were complete output hallucinates.
        let mut r = kaish_kernel::interpreter::ExecResult::success(
            "head…\n[output truncated: spilled to /v/spill/abc]",
        );
        r.did_spill = true;
        r.original_code = Some(r.code);
        r.code = 3;
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(!kr.is_error, "a spilled successful command is not an error");
        let structured = kr.structured.unwrap();
        assert_eq!(
            structured["exit_code"],
            serde_json::json!(0),
            "envelope carries the command's real exit, not kaish's spill marker"
        );
        assert_eq!(structured["status"], serde_json::json!("done"));
        assert_eq!(
            structured["did_spill"],
            serde_json::json!(true),
            "truncation stays unmissable as a field"
        );
    }

    #[test]
    fn conversion_spilled_failure_stays_error_with_original_code() {
        let mut r = kaish_kernel::interpreter::ExecResult::failure(3, "tail of a real failure");
        r.did_spill = true;
        r.original_code = Some(1);
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(kr.is_error, "a spilled FAILING command is still an error");
        let structured = kr.structured.unwrap();
        assert_eq!(
            structured["exit_code"],
            serde_json::json!(1),
            "the real code, not the spill marker"
        );
        assert_eq!(structured["status"], serde_json::json!("error"));
        assert_eq!(structured["did_spill"], serde_json::json!(true));
    }

    #[test]
    fn conversion_unspilled_results_report_did_spill_false() {
        let plain = envelope_of(kaish_kernel::interpreter::ExecResult::success("ok"));
        assert_eq!(
            plain["did_spill"],
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
        broker.set_binding(ctx_id, binding).await.unwrap();

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
        broker.set_binding(ctx_id, binding).await.unwrap();

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
        broker.set_binding(ctx_id, binding).await.unwrap();

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hello-ro"), &cc, CancellationToken::new())
            .await
            .expect("safe shell call should succeed");

        assert!(!result.is_error, "echo should not be an error: {result:?}");
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("hello-ro"), "stdout missing, got: {out:?}");
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
        broker.set_binding(ctx_id, binding).await.unwrap();
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
            _ => {
                let out = streams_of(&result);
                assert!(out.contains("still-works"), "got: {out:?}");
            }
        }
    }

    /// **Slice 3 spec test 2**: a context newly granted `facade:shell_write`
    /// (plus the `exec` authority — external spawning is gated on that
    /// authority independent of which facade is granted, see
    /// `EmbeddedKaish::for_context`) gets exactly
    /// what `builtin.shell` provided before the flag day, under the new name
    /// — proven with a real external binary (`id`), not just a kaish
    /// builtin, so the assertion actually exercises `ExternalExec::Allow`,
    /// not merely that a command ran.
    ///
    /// The `exec` grant must land on TWO brokers: `wired()`'s standalone
    /// broker (what `call_tool` gates against) AND `d.kernel().broker()`
    /// (what `EmbeddedKaish::for_context`'s exec-authority check reads,
    /// via `self.kernel().broker()` — a different `Arc<Broker>` than the one
    /// `ShellServer` was registered on for the *synchronous* path; the
    #[tokio::test]
    async fn shell_write_grant_gets_full_external_exec_under_the_new_name() {
        let (broker, d) = wired().await;
        // Real host root so the shell's default cwd resolves to a real
        // directory and `id` can actually spawn (mirrors
        // `runtime/context_shell.rs`'s `unknown_command_fails_fast_exec_granted_shell`).
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let ctx_id = crate::kj::test_helpers::register_rooted_context(&d, Some("write-exec"), principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding.clone()).await.unwrap();
        d.kernel().broker().set_binding(ctx_id, binding).await.unwrap();
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));

        let pending = broker
            .call_tool(call_write("id"), &cc, CancellationToken::new())
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "an uncovered submission escalates to an open ask: {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), true);

        let result = broker
            .call_tool(call_write("id"), &cc, CancellationToken::new())
            .await
            .expect("shell_write call should succeed once the pending ask is answered");
        assert!(!result.is_error, "`id` should run and exit 0: {result:?}");
        let out = streams_of(&result);
        assert!(
            out.contains("uid="),
            "`id` must have actually spawned as a real external process, got: {out:?}"
        );
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
        broker.set_binding(ctx_id, binding).await.unwrap();
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let result = broker
            .call_tool(call("id"), &cc, CancellationToken::new())
            .await
            .expect("the call itself succeeds structurally — the shell runs, `id` just can't spawn");
        let out = streams_of(&result);
        assert!(
            !out.contains("uid="),
            "a stale `shell` request must NEVER reach ExternalExec::Allow \
             and spawn a real process, got: {out:?}"
        );
    }

    async fn wait_for_operation(
        d: &Arc<crate::kj::KjDispatcher>,
        context: kaijutsu_types::ContextId,
        operation_id: &str,
    ) -> crate::shell_operations::ShellOperationState {
        let started = std::time::Instant::now();
        loop {
            let state = d
                .kernel()
                .shell_operations()
                .get(operation_id, context)
                .expect("operation lookup")
                .expect("operation receipt remains durable");
            if state.completed_at.is_some() {
                return state;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(5),
                "timed out waiting for shell operation {operation_id}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn worker_shutdown_settles_running_shell_jobs() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("shutdown-tool"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let receipt = broker.call_tool(call_async("echo started; sleep 30; echo never"), &cc, CancellationToken::new()).await.unwrap();
        let id = body_of(&receipt)["operation_id"].as_str().unwrap().to_owned();
        let state = d.kernel().shell_operations().get(&id, context).unwrap().unwrap();
        let jobs = d.kernel().context_job_manager(context);
        let job = jobs.list().await.into_iter().find(|job| Some(job.id.to_string()) == state.receipt.job_id).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while jobs.read_stdout(job.id).await.unwrap().is_empty() { tokio::task::yield_now().await; }
        }).await.unwrap();
        d.kernel().stop_runtime_worker();
        let state = wait_for_operation(&d, context, &id).await;
        let envelope = state.envelope.unwrap();
        assert!(envelope.is_error());
        assert_eq!(envelope.exit_code, Some(130), "preserve kaish's cancellation result");
        assert!(!envelope.stdout.contains("never"));
        let outcome = d.kernel().shell_operations().outcome(&id, context).unwrap().unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), jobs.wait(job.id)).await.unwrap().unwrap();
        assert_eq!(result, outcome.exec_result(), "job and durable result must agree");
        let streams = jobs.streams(job.id).await.unwrap();
        assert!(streams.stdout.is_closed().await && streams.stderr.is_closed().await);
        assert_eq!(streams.stdout.read().await, b"started\n");
        for block in [&state.receipt.command_block_id, &state.receipt.output_block_id] {
            assert_eq!(d.block_store().get_block_snapshot(context, block).unwrap().unwrap().status, kaijutsu_types::Status::Error);
        }
    }

    #[tokio::test]
    async fn asynchronous_shell_keeps_live_statement_output() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("live-tool"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let receipt = broker.call_tool(call_async("echo first; sleep 30; echo last"), &cc, CancellationToken::new()).await.unwrap();
        let id = body_of(&receipt)["operation_id"].as_str().unwrap().to_owned();
        let state = d.kernel().shell_operations().get(&id, context).unwrap().unwrap();
        let jobs = d.kernel().context_job_manager(context);
        let job = jobs.list().await.into_iter().find(|job| Some(job.id.to_string()) == state.receipt.job_id).unwrap();
        let progress = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let output = jobs.read_stdout(job.id).await.unwrap();
                if !output.is_empty() { break output; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await;
        assert!(d.kernel().shell_operations().get(&id, context).unwrap().unwrap().completed_at.is_none());
        assert!(d.kernel().shell_operations().cancel(&id, context).await.unwrap());
        wait_for_operation(&d, context, &id).await;
        assert_eq!(progress.expect("running jobs expose completed statement output"), b"first\n");
    }

    #[tokio::test]
    async fn archived_context_refuses_both_shell_tools_without_receipts() {
        for read_only in [true, false] {
            for foreground in [true, false] {
                let (broker, d) = wired().await;
                let principal = PrincipalId::new();
                let context = crate::kj::test_helpers::register_rooted_context(&d, Some("archived-tool"), principal);
                d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
                let mut binding = ContextToolBinding::new();
                binding.grant(Capability::Facade(if read_only { "shell" } else { "shell_write" }.into()));
                broker.set_binding(context, binding).await.unwrap();
                d.kernel_db().lock().archive_context(context).unwrap();
                let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id())
                    .with_actor(principal, Some(PrincipalId::new()));
                let mut params = if read_only { call("echo must-not-run") } else { call_write("echo must-not-run") };
                params.arguments["foreground"] = serde_json::json!(foreground);
                let result = broker.call_tool(params, &cc, CancellationToken::new()).await;
                d.kernel().shutdown_runtime_worker().await.unwrap();
                let error = result.expect_err("archived context cannot start a new shell tool");
                assert!(error.to_string().contains("archived"), "{error}");
                assert!(d.kernel().shell_operations().list_for_context(context).unwrap().is_empty());
                assert!(d.block_store().block_snapshots(context).unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn shell_state_writeback_respects_read_only_policy() {
        for read_only in [true, false] {
            for foreground in [true, false] {
                let (broker, d) = wired().await;
                let principal = PrincipalId::new();
                let context = crate::kj::test_helpers::register_rooted_context(&d, Some("tool-state"), principal);
                d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
                let mut binding = ContextToolBinding::new();
                binding.grant(Capability::Facade(if read_only { "shell" } else { "shell_write" }.into()));
                broker.set_binding(context, binding).await.unwrap();
                let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id())
                    .with_actor(principal, Some(PrincipalId::new()));
                let code = "export TOOL_STATE=kept; echo $TOOL_STATE";
                if !read_only {
                    let pending = broker.call_tool(call_write(code), &cc, CancellationToken::new()).await.unwrap_err();
                    assert!(matches!(pending, McpError::Refused(ref r) if r.kind == RefusalKind::Pending));
                    answer_pending_ask(d.kernel_db().clone(), true);
                }
                let mut params = if read_only { call(code) } else { call_write(code) };
                params.arguments["foreground"] = foreground.into();
                let result = broker.call_tool(params, &cc, CancellationToken::new()).await.unwrap();
                let body = body_of(&result);
                let envelope = if foreground { body } else {
                    wait_for_operation(&d, context, body["operation_id"].as_str().unwrap()).await.envelope.unwrap().to_value()
                };
                assert_eq!(envelope["stdout"], "kept\n");
                let env = d.kernel_db().lock().get_context_env(context).unwrap();
                assert_eq!(env.iter().find(|row| row.key == "TOOL_STATE").map(|row| row.value.as_str()),
                    if read_only { None } else { Some("kept") });
            }
        }
    }

    struct AfterCallerDrop(Arc<tokio::sync::Notify>);

    #[tokio::test]
    async fn worker_shutdown_settles_a_paused_result_hook() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId, GlobPattern};
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("shutdown-hook"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        broker.hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("shutdown-hook".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell".into())), match_context: Some(context), match_principal: None,
            priority: 0, kaish_script_id: None, action: HookAction::Invoke(HookBody::Builtin {
                name: "shutdown-hook".into(), hook: Arc::new(ShutdownHook { entered: entered.clone(), release: release.clone() }) }),
        });
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let receipt = broker.call_tool(call_async("echo captured"), &cc, CancellationToken::new()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()).await.unwrap();
        d.kernel().stop_runtime_worker();
        let id = body_of(&receipt)["operation_id"].as_str().unwrap().to_owned();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(outcome) = d.kernel().shell_operations().outcome(&id, context).unwrap() { break outcome; }
                tokio::task::yield_now().await;
            }
        }).await;
        release.notify_one();
        let outcome = settled.expect("shutdown must interrupt a hook that never returns");
        assert!(outcome.envelope().is_error());
        let crate::runtime::command_outcome::CommandExecution::Completed(raw) = outcome.execution
            else { panic!("shutdown discarded captured execution") };
        assert_eq!(raw.text_out(), "captured\n");
        wait_for_operation(&d, context, &id).await;
    }

    struct ShutdownHook {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl crate::mcp::Hook for ShutdownHook {
        async fn invoke(&self, _: &KernelCallParams, _: &CallContext) -> McpResult<()> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    #[async_trait]
    impl crate::mcp::Hook for AfterCallerDrop {
        async fn invoke(&self, _: &KernelCallParams, _: &CallContext) -> McpResult<()> {
            self.0.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn asynchronous_shell_outlives_the_submitting_runtime() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId, GlobPattern};
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("detached-tool"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        broker.hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("after-caller-drop".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell".into())), match_context: Some(context), match_principal: None,
            priority: 0, kaish_script_id: None, action: HookAction::Invoke(HookBody::Builtin {
                name: "after-caller-drop".into(), hook: Arc::new(AfterCallerDrop(release.clone())) }),
        });
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let (sent, received) = std::sync::mpsc::channel();
        crate::spawn_kaish_thread("tool-caller-test", move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let receipt = rt.block_on(broker.call_tool(call_async("echo caller-gone"), &cc, CancellationToken::new())).unwrap();
            sent.send(receipt).unwrap();
        }).unwrap().join().unwrap();
        let receipt = received.recv().unwrap();
        release.notify_one();
        let id = body_of(&receipt)["operation_id"].as_str().unwrap().to_owned();
        let result = wait_for_operation(&d, context, &id).await.envelope.unwrap();
        assert_eq!(result.stdout, "caller-gone\n");
        assert_eq!(result.status, ShellStatus::Done);
    }

    struct ReenterShell {
        broker: Weak<Broker>,
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::mcp::Hook for ReenterShell {
        async fn invoke(&self, params: &KernelCallParams, ctx: &CallContext) -> McpResult<()> {
            let count = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if count >= 8 { return Err(McpError::Protocol("test stopped unbounded shell hook recursion".into())); }
            let result = self.broker.upgrade().unwrap().call_tool(params.clone(), ctx, CancellationToken::new()).await?;
            if result.is_error { Err(McpError::Protocol("nested shell tool failed".into())) } else { Ok(()) }
        }
    }

    #[tokio::test]
    async fn retained_shell_workers_preserve_the_hook_recursion_limit() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId, GlobPattern};
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("shell-hook-depth"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        broker.hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("reenter-shell".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell".into())), match_context: Some(context), match_principal: None,
            priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "reenter-shell".into(),
                hook: Arc::new(ReenterShell { broker: Arc::downgrade(&broker), count: count.clone() }) }),
        });
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let result = broker.call_tool(call("echo depth"), &cc, CancellationToken::new()).await;
        assert!(count.load(std::sync::atomic::Ordering::SeqCst) <= crate::mcp::broker::MAX_HOOK_DEPTH as usize,
            "moving execution to a worker must not reset recursive hook depth");
        assert!(result.is_err());
    }

    struct PanickingResultHook;

    #[async_trait]
    impl crate::mcp::Hook for PanickingResultHook {
        async fn invoke(&self, _: &KernelCallParams, _: &CallContext) -> McpResult<()> {
            panic!("post-review hook panic sentinel");
        }
    }

    #[tokio::test]
    async fn result_review_checkpoint_failure_leaves_no_actionable_ask() {
        use crate::mcp::{HookAction, HookEntry, HookId, GlobPattern, AskSpec};
        for foreground in [false, true] {
            for table in ["shell_result_reviews", "shell_result_review_asks"] {
                let (broker, d) = wired().await;
                let principal = PrincipalId::new();
                let context = crate::kj::test_helpers::register_rooted_context(&d, Some("failed-review-admission"), principal);
                d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
                let mut binding = ContextToolBinding::new();
                binding.grant(Capability::Facade("shell".into()));
                broker.set_binding(context, binding).await.unwrap();
                broker.hooks().write().await.post_call.entries.push(HookEntry {
                    id: HookId("failed-review-checkpoint".into()), match_instance: None,
                    match_tool: Some(GlobPattern("shell".into())), match_context: Some(context), match_principal: None,
                    action: HookAction::Ask(AskSpec { description: Some("Review captured output".into()) }),
                    priority: 0, kaish_script_id: None,
                });
                d.kernel_db().lock().conn_for_ledger().execute_batch(&format!(
                    "CREATE TRIGGER fail_checkpoint BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'injected checkpoint fault'); END;"
                )).unwrap();
                let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
                let params = if foreground { call("echo captured") } else { call_async("echo captured") };
                let result = broker.call_tool(params, &cc, CancellationToken::new()).await;
                if foreground {
                    let Err(McpError::Refused(refusal)) = result else { panic!("failed checkpoint must refuse review") };
                    assert!(refusal.reason.contains("injected checkpoint fault"), "{refusal:?}");
                } else {
                    let id = body_of(&result.unwrap())["operation_id"].as_str().unwrap().to_owned();
                    let state = wait_for_operation(&d, context, &id).await;
                    assert!(state.envelope.unwrap().is_error());
                    let raw = d.kernel().shell_operations().outcome(&id, context).unwrap().unwrap();
                    let crate::runtime::command_outcome::CommandExecution::Completed(result) = raw.execution else { panic!("lost capture") };
                    assert_eq!(result.text_out(), "captured\n");
                }
                let asks: i64 = d.kernel_db().lock().conn_for_ledger().query_row(
                    "SELECT COUNT(*) FROM approvals WHERE origin='hook_result'", [], |row| row.get(0)).unwrap();
                assert_eq!(asks, 0, "checkpoint failure must roll back its ask, including abandoned rows");
                let reviews: i64 = d.kernel_db().lock().conn_for_ledger().query_row(
                    "SELECT COUNT(*) FROM shell_result_reviews", [], |row| row.get(0)).unwrap();
                assert_eq!(reviews, 0, "link failure must roll back its captured review too");
                d.kernel_db().lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_checkpoint").unwrap();
                d.kernel().shutdown_runtime_worker().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn tool_result_reviews_retain_execution_for_foreground_and_background_calls() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId, GlobPattern, AskSpec};
        for foreground in [false, true] {
            for decision in ["deny", "allow", "cancel", "shutdown", "panic"] {
                let allow = decision == "allow";
                let (broker, d) = wired().await;
                let principal = PrincipalId::new();
                let reviewer = PrincipalId::new();
                let context = crate::kj::test_helpers::register_rooted_context(&d, Some("tool-review"), principal);
                d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
                d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
                    principal_id: reviewer, name: "tool-reviewer".into(), created_at: 0, retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
                let mut binding = ContextToolBinding::new();
                binding.grant(Capability::Facade("shell".into()));
                broker.set_binding(context, binding).await.unwrap();
                let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id()).with_actor(principal, Some(reviewer));
                let mut hooks = broker.hooks().write().await;
                for (id, action, priority) in [
                    ("tool-review", HookAction::Ask(AskSpec { description: Some("Review captured shell".into()) }), 0),
                    ("after-review", if decision == "panic" {
                        HookAction::Invoke(HookBody::Builtin { name: "panic-test".into(), hook: Arc::new(PanickingResultHook) })
                    } else { HookAction::ShortCircuit(KernelToolResult::text("reviewed tool output")) }, 1),
                ] {
                    hooks.post_call.entries.push(HookEntry { id: HookId(id.into()), match_instance: None,
                        match_tool: Some(GlobPattern("shell".into())), match_context: Some(context), match_principal: None,
                        action, priority, kaish_script_id: None });
                }
                drop(hooks);
                let params = if foreground { call("echo captured") } else { call_async("echo captured") };
                let cancel = CancellationToken::new();
                let result = tokio::time::timeout(std::time::Duration::from_secs(5),
                    broker.call_tool(params, &cc, cancel.clone())).await.expect("review releases the tool call");
                let operation = if foreground {
                    assert!(matches!(result, Err(McpError::Refused(ref r)) if r.kind == RefusalKind::Pending));
                    None
                } else { Some(body_of(&result.unwrap())["operation_id"].as_str().unwrap().to_owned()) };
                let ask = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let asks = d.kernel_db().lock().list_pending_asks().unwrap();
                        if let Some(ask) = asks.into_iter().find(|row| row.hook_id.as_deref() == Some("tool-review"))
                            && d.kernel().shell_operations().result_review_for_ask(&ask.request_id, context).unwrap().is_some() {
                            break ask;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }).await.unwrap();
                assert!(ask.exec_source.is_none());
                let record = d.kernel().shell_operations().result_review_for_ask(&ask.request_id, context).unwrap().unwrap();
                assert_eq!(record.operation_id, operation);
                assert!(record.settled.is_none());
                if decision == "shutdown" {
                    d.kernel().stop_runtime_worker();
                } else if decision == "cancel" {
                    if let Some(operation) = &operation {
                        assert!(d.kernel().shell_operations().cancel(operation, context).await.unwrap());
                    } else { cancel.cancel(); }
                } else { answer_pending_ask(d.kernel_db().clone(), allow || decision == "panic"); }
                let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        if let Some(outcome) = d.kernel().shell_operations().result_review_for_ask(&ask.request_id, context)
                            .unwrap().unwrap().settled { break outcome; }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }).await.unwrap();
                if matches!(decision, "cancel" | "shutdown") {
                    assert_eq!(d.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
                        approval_ledger::types::ApprovalStatus::Abandoned);
                    assert!(d.kernel_db().lock().redeem_ask(&ask.request_id).is_err(), "cancellation must not authorize anything");
                }
                let envelope = settled.envelope();
                let expected_job = settled.exec_result();
                assert_eq!(envelope.is_error(), !allow);
                assert_eq!(envelope.stdout, if allow { "reviewed tool output" } else { "" });
                let crate::runtime::command_outcome::CommandExecution::Completed(raw) = settled.execution
                    else { panic!("lost captured tool execution") };
                assert_eq!(raw.text_out(), "captured\n");
                if decision == "panic" { assert!(d.kernel().shutdown_runtime_worker().await.is_err()); }
                if let Some(operation) = operation {
                    let state = wait_for_operation(&d, context, &operation).await;
                    let jobs = d.kernel().context_job_manager(context);
                    let job = jobs.list().await.into_iter().find(|job| Some(job.id.to_string()) == state.receipt.job_id).unwrap();
                    let actual = tokio::time::timeout(std::time::Duration::from_secs(3), jobs.wait(job.id)).await.unwrap().unwrap();
                    assert_eq!(actual, expected_job, "job completion must preserve the retained review outcome");
                }
                else { assert!(d.block_store().block_snapshots(context).unwrap().is_empty()); }
            }
        }
    }

    #[tokio::test]
    async fn result_hooks_settle_the_command_instead_of_replacing_its_receipt() {
        use crate::mcp::{HookAction, HookEntry, HookId, GlobPattern};
        for (foreground, input) in [(false, ""), (false, "captured input"), (true, "captured input")] {
            let (broker, d) = wired().await;
            let principal = PrincipalId::new();
            let context = register_context(&d, Some("settled-tool"), None, principal);
            d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
            let mut binding = ContextToolBinding::new();
            binding.grant(Capability::Facade("shell".into()));
            broker.set_binding(context, binding).await.unwrap();
            let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
            let replacement = serde_json::json!({"reviewed": true});
            broker.hooks().write().await.post_call.entries.push(HookEntry {
                id: HookId("replace-shell-result".into()), match_instance: Some(GlobPattern(ShellServer::INSTANCE.into())),
                match_tool: Some(GlobPattern("shell".into())), match_context: Some(context), match_principal: None,
                priority: 0, kaish_script_id: None,
                action: HookAction::ShortCircuit(KernelToolResult { is_error: false,
                    content: vec![ToolContent::Text("replacement".into())], structured: Some(replacement.clone()) }),
            });
            let params = KernelCallParams { instance: InstanceId::new(ShellServer::INSTANCE), tool: "shell".into(),
                arguments: serde_json::json!({"command": "cat", "stdin": input, "foreground": foreground}) };
            let result = broker.call_tool(params, &cc, CancellationToken::new()).await.unwrap();
            let body = body_of(&result);
            let settled = if foreground { body } else {
                assert_eq!(body["status"], "running", "PostCall must not replace an admission receipt");
                let id = body["operation_id"].as_str().unwrap();
                let state = wait_for_operation(&d, context, id).await;
                let captured = d.kernel().shell_operations().outcome(id, context).unwrap().unwrap();
                let crate::runtime::command_outcome::CommandExecution::Completed(raw) = captured.execution
                    else { panic!("lost captured execution") };
                assert_eq!(raw.text_out(), input);
                let jobs = d.kernel().context_job_manager(context);
                let job = jobs.list().await.into_iter().find(|job| Some(job.id.to_string()) == state.receipt.job_id).unwrap();
                assert_eq!(jobs.read_stdout(job.id).await.unwrap(), input.as_bytes(), "job streams retain raw observations");
                assert_eq!(jobs.wait(job.id).await.unwrap().text_out(), "replacement", "final job result honors hooks");
                let output = d.block_store().get_block_snapshot(context, &state.receipt.output_block_id).unwrap().unwrap();
                assert_eq!(output.content, "replacement");
                assert_eq!(output.output.unwrap().rich_json, Some(replacement.clone()));
                state.envelope.unwrap().to_value()
            };
            assert_eq!(settled["stdout"], "replacement");
            assert_eq!(settled["data"], replacement);
            assert!(settled["exit_code"].is_null(), "replacement has no physical exit");
        }
    }

    #[tokio::test]
    async fn async_default_runs_the_full_kaish_program_and_records_one_operation() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("async-program"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let epoch = d.kernel_db().lock().begin_continuation(context, 1).unwrap().epoch;
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());

        let receipt = broker.call_tool(call_async("echo first; echo second"), &cc, CancellationToken::new())
            .await.expect("async safe shell starts a kaish program");
        assert!(!receipt.is_error, "a receipt is not command failure: {receipt:?}");
        let body = body_of(&receipt);
        assert_eq!(body["status"], serde_json::json!("running"));
        let operation_id = body["operation_id"].as_str().expect("stable operation id").to_owned();
        assert!(body["ask_id"].is_null(), "safe builtin needs no approval");
        let state = d.kernel().shell_operations().get(&operation_id, context).unwrap().unwrap();
        assert_eq!(state.continuation_epoch, Some(epoch));

        let completed = wait_for_operation(&d, context, &operation_id).await;
        let envelope = completed.envelope.expect("completion envelope");
        assert_eq!(envelope.status, ShellStatus::Done);
        assert!(envelope.stdout.contains("first"));
        assert!(envelope.stdout.contains("second"));
        assert_eq!(d.kernel().shell_operations().list_for_context(context).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn async_safe_shell_denies_external_commands_without_host_fallback() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("async-safe-external"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());

        let receipt = broker.call_tool(call_async("id"), &cc, CancellationToken::new())
            .await.expect("the async receipt is returned before kaish rejects external dispatch");
        let operation_id = body_of(&receipt)["operation_id"].as_str().unwrap().to_owned();
        let completed = wait_for_operation(&d, context, &operation_id).await;
        let envelope = completed.envelope.unwrap();
        assert!(envelope.is_error());
        assert!(!envelope.stdout.contains("uid="), "safe shell must not spawn id");
        assert!(envelope.stderr.contains("external") || envelope.stdout.contains("external"),
            "kaish must name the configured external-command refusal: {envelope:?}");
    }

    #[tokio::test]
    async fn pending_async_shell_write_has_a_durable_waiting_operation_and_ask() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let context = register_context(&d, Some("async-pending"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        d.kernel_db().lock().insert_character(&crate::kernel_db::CharacterRow {
            principal_id: reviewer, name: "reviewer".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        d.kernel_db().lock().update_context_review(context, Some(principal), Some(reviewer)).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id())
            .with_actor(principal, Some(reviewer));

        let pending = broker.call_tool(call_write_async("echo waits-for-approval"), &cc, CancellationToken::new())
            .await.expect("uncovered async write returns a durable waiting receipt");
        assert!(!pending.is_error, "waiting for approval is not execution failure: {pending:?}");
        let pending_body = body_of(&pending);
        assert_eq!(pending_body["status"], serde_json::json!("waiting"));
        let ask_id = pending_body["ask_id"].as_str().expect("ask id");
        assert!(d.kernel_db().lock().approval_pair_expected(ask_id).unwrap());
        assert!(d.kernel_db().lock().approval_pair_ready(ask_id).unwrap());
        let operation = d.kernel().shell_operations().get_by_ask(ask_id, context).unwrap()
            .expect("pending ask has a durable operation receipt");
        assert_eq!(operation.receipt.ask_id.as_deref(), Some(ask_id));
        assert!(operation.receipt.job_id.is_none(), "a pending ask must not start kaish");
        assert!(operation.completed_at.is_none());
        let output = d.block_store().get_block_snapshot(context, &operation.receipt.output_block_id).unwrap().unwrap();
        assert_eq!(output.status, kaijutsu_types::Status::Waiting);
    }

    /// A context shares one kaish JobManager across ephemeral shell
    /// materializations. The read-only façade must never turn that shared
    /// manager into a cancellation handle for another operation.
    #[tokio::test]
    async fn safe_shell_cannot_cancel_a_context_operation_through_kaish_job_control() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("safe-job-control"), None, principal);
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());

        let receipt = broker.call_tool(call_async("sleep 2"), &cc, CancellationToken::new())
            .await.expect("safe shell may run a read-only builtin asynchronously");
        let operation_id = body_of(&receipt)["operation_id"].as_str().unwrap().to_owned();
        let state = d.kernel().shell_operations().get(&operation_id, context).unwrap().unwrap();
        let job_id = state.receipt.job_id.expect("operation is attached to shared kaish job");

        let attempt = broker.call_tool(call(&format!("kill %{job_id}")), &cc, CancellationToken::new())
            .await.expect("job-control refusal is a shell result");
        assert!(attempt.is_error, "read-only shell must refuse cancellation through the shared manager");
        assert!(streams_of(&attempt).contains("job control") || streams_of(&attempt).contains("read-only"),
            "the refusal must name its capability boundary: {attempt:?}");
        let still_running = d.kernel().shell_operations().get(&operation_id, context).unwrap().unwrap();
        assert!(still_running.completed_at.is_none(), "a read-only job-control attempt must not cancel the operation");

        let write_attempt = broker.call_tool(
            call(&format!("echo altered > /v/jobs/{job_id}/stdout")),
            &cc,
            CancellationToken::new(),
        ).await.expect("read-only jobfs refusal is a shell result");
        assert!(write_attempt.is_error, "a safe shell must not write the shared job filesystem");
        assert!(streams_of(&write_attempt).contains("read-only") || streams_of(&write_attempt).contains("Permission denied"),
            "jobfs write must fail loudly: {write_attempt:?}");
        d.kernel().shell_operations().cancel(&operation_id, context).await.unwrap();
    }

    #[tokio::test]
    async fn foreground_true_keeps_the_completed_result_contract() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("explicit-foreground"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(context, binding).await.unwrap();
        let cc = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let result = broker.call_tool(call("echo foreground"), &cc, CancellationToken::new()).await.unwrap();
        assert_eq!(body_of(&result)["status"], serde_json::json!("done"));
        assert!(streams_of(&result).contains("foreground"));
        assert!(body_of(&result)["operation_id"].is_null());
    }

}
