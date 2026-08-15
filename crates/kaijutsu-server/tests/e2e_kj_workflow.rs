//! End-to-end integration tests for the fork → work → drift → merge workflow.
//!
//! Exercises the full SSH + Cap'n Proto stack with a mock LLM provider.
//! Each test starts a fresh ephemeral server + client.

mod common;
use common::*;

use kaijutsu_client::KernelHandle;
use kaijutsu_types::{BlockId, BlockKind, BlockQuery, BlockSnapshot, ContextId, Role, Status};

// ============================================================================
// Test helpers
// ============================================================================

/// Execute a shell command and poll until the output block reaches a terminal status.
///
/// Returns `(command_block_id, output_content, output_status)`.
/// Panics on timeout (default 10s).
async fn shell_exec_wait(
    kernel: &KernelHandle,
    code: &str,
    context_id: ContextId,
) -> (BlockId, String, Status) {
    shell_exec_wait_timeout(kernel, code, context_id, 10_000).await
}

async fn shell_exec_wait_timeout(
    kernel: &KernelHandle,
    code: &str,
    context_id: ContextId,
    timeout_ms: u64,
) -> (BlockId, String, Status) {
    let cmd_block_id = kernel
        .shell_execute(code, context_id, false)
        .await
        .unwrap_or_else(|e| panic!("shell_execute({code:?}) failed: {e}"));

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        if std::time::Instant::now() > deadline {
            // Fetch blocks one final time for diagnostic output
            let blocks = kernel
                .get_blocks(context_id, &BlockQuery::All)
                .await
                .unwrap_or_default();
            panic!(
                "shell_exec_wait({code:?}) timed out after {timeout_ms}ms.\n\
                 cmd_block_id={cmd_block_id:?}\n\
                 blocks ({} total): {blocks:#?}",
                blocks.len()
            );
        }

        // Brief yield to let spawn_local tasks run (same thread)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let blocks = kernel
            .get_blocks(context_id, &BlockQuery::All)
            .await
            .unwrap_or_else(|e| panic!("get_blocks failed while polling {code:?}: {e}"));

        // Find the ToolResult block whose parent is our command block
        if let Some(output) = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_block_id))
        {
            match output.status {
                Status::Done | Status::Error => {
                    return (cmd_block_id, output.content.clone(), output.status);
                }
                _ => {
                    // Still running, keep polling
                }
            }
        }
    }
}

/// Get all blocks in a context.
async fn get_all_blocks(kernel: &KernelHandle, context_id: ContextId) -> Vec<BlockSnapshot> {
    kernel
        .get_blocks(context_id, &BlockQuery::All)
        .await
        .unwrap_or_else(|e| panic!("get_all_blocks({context_id}) failed: {e}"))
}

// ============================================================================
// Core E2E: fork → work → drift → merge
// ============================================================================

#[test]
fn test_fork_work_drift_merge_e2e() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;

        // Attach kernel
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // Create root context "main" and join it
        let main_ctx = kernel.create_context("main").await.unwrap();
        let _joined = kernel.join_context(main_ctx, "test").await.unwrap();

        // Work: run a command in main
        let (_cmd_id, output, status) =
            shell_exec_wait(&kernel, "echo 'initial work'", main_ctx).await;
        assert_eq!(status, Status::Done, "echo failed: {output}");
        assert!(
            output.contains("initial work"),
            "expected 'initial work' in output, got: {output}"
        );

        // Fork: create exploration context via kj
        let (_cmd_id, fork_output, fork_status) =
            shell_exec_wait(&kernel, "kj fork --name exploration", main_ctx).await;
        assert_eq!(fork_status, Status::Done, "kj fork failed: {fork_output}");
        assert!(
            fork_output.contains("exploration"),
            "expected 'exploration' in fork output, got: {fork_output}"
        );

        // Verify both contexts exist
        let contexts = kernel.list_contexts().await.unwrap();
        let exploration = contexts
            .iter()
            .find(|c| c.label == "exploration")
            .expect("exploration context not found in list");
        let exploration_id = exploration.id;
        assert!(
            contexts.iter().any(|c| c.label == "main"),
            "main context not found in list"
        );

        // Join the exploration context so we can operate in it
        let _joined = kernel.join_context(exploration_id, "test").await.unwrap();

        // Switch active context to exploration
        let (_cmd_id, switch_output, switch_status) =
            shell_exec_wait(&kernel, "kj context switch exploration", main_ctx).await;
        assert_eq!(
            switch_status,
            Status::Done,
            "kj context switch failed: {switch_output}"
        );

        // Work in fork
        let (_cmd_id, work_output, work_status) =
            shell_exec_wait(&kernel, "echo 'found the bug'", exploration_id).await;
        assert_eq!(
            work_status,
            Status::Done,
            "echo in fork failed: {work_output}"
        );

        // Drift push: send content to main. Since 2026-08-12 `push` DELIVERS
        // rather than staging — staging moved behind `--stage`. This test used
        // to assert "staged" here and then rely on the flush below to land the
        // block; both halves are now checked for the behaviour they actually
        // have.
        let (_cmd_id, push_output, push_status) = shell_exec_wait(
            &kernel,
            r#"kj drift push main "auth bypass in login""#,
            exploration_id,
        )
        .await;
        assert_eq!(
            push_status,
            Status::Done,
            "kj drift push failed: {push_output}"
        );
        assert!(
            push_output.to_lowercase().contains("drifted"),
            "expected 'drifted' in push output, got: {push_output}"
        );

        // The contract worth pinning end-to-end: the block is in the target's
        // document as soon as push returns, with no flush in between.
        let after_push = get_all_blocks(&kernel, main_ctx).await;
        assert!(
            after_push
                .iter()
                .any(|b| b.kind == BlockKind::Drift && b.content.contains("auth bypass")),
            "push must deliver without a flush; blocks: {:?}",
            after_push
                .iter()
                .map(|b| (&b.kind, &b.content))
                .collect::<Vec<_>>()
        );

        // ...and flush therefore has nothing left to do. This is the assertion
        // that would have caught the old behaviour surviving.
        let (_cmd_id, flush_output, flush_status) =
            shell_exec_wait(&kernel, "kj drift flush", exploration_id).await;
        assert_eq!(
            flush_status,
            Status::Done,
            "kj drift flush failed: {flush_output}"
        );
        assert!(
            flush_output.to_lowercase().contains("nothing to flush"),
            "push already delivered, so flush should have nothing staged, got: {flush_output}"
        );

        // Verify drift landed in main
        let main_blocks = get_all_blocks(&kernel, main_ctx).await;
        let drift_block = main_blocks.iter().find(|b| b.kind == BlockKind::Drift);
        assert!(
            drift_block.is_some(),
            "expected a Drift block in main context, blocks: {:?}",
            main_blocks
                .iter()
                .map(|b| (&b.kind, &b.content))
                .collect::<Vec<_>>()
        );
        assert!(
            drift_block.unwrap().content.contains("auth bypass"),
            "drift content should contain 'auth bypass', got: {}",
            drift_block.unwrap().content
        );

        // Context tree listing
        let (_cmd_id, list_output, list_status) =
            shell_exec_wait(&kernel, "kj context list", exploration_id).await;
        assert_eq!(
            list_status,
            Status::Done,
            "kj context list failed: {list_output}"
        );
        assert!(
            list_output.contains("main") && list_output.contains("exploration"),
            "context list should show both contexts, got: {list_output}"
        );
    });
}

// ============================================================================
// Variant: drift push/flush between siblings
// ============================================================================

#[test]
fn test_drift_push_flush_between_siblings_e2e() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;

        let (kernel, _) = client.bind_kernel().await.unwrap();

        // Create two sibling contexts
        let alpha_id = kernel.create_context("alpha").await.unwrap();
        let beta_id = kernel.create_context("beta").await.unwrap();
        kernel.join_context(alpha_id, "test").await.unwrap();
        kernel.join_context(beta_id, "test").await.unwrap();

        // Switch to alpha
        let (_cmd_id, _, status) =
            shell_exec_wait(&kernel, "kj context switch alpha", alpha_id).await;
        assert_eq!(status, Status::Done);

        // Push from alpha to beta
        let (_cmd_id, push_out, push_status) = shell_exec_wait(
            &kernel,
            r#"kj drift push beta "hello from alpha""#,
            alpha_id,
        )
        .await;
        assert_eq!(push_status, Status::Done, "push failed: {push_out}");

        // Flush
        let (_cmd_id, flush_out, flush_status) =
            shell_exec_wait(&kernel, "kj drift flush", alpha_id).await;
        assert_eq!(flush_status, Status::Done, "flush failed: {flush_out}");

        // Verify drift in beta
        let beta_blocks = get_all_blocks(&kernel, beta_id).await;
        let drift = beta_blocks
            .iter()
            .find(|b| b.kind == BlockKind::Drift)
            .expect("expected Drift block in beta");
        assert!(
            drift.content.contains("hello from alpha"),
            "drift content mismatch: {}",
            drift.content
        );
    });
}

// ============================================================================
// Variant: two clients same kernel
// ============================================================================

#[test]
fn test_two_clients_same_kernel_e2e() {
    run_local(async {
        let addr = start_server().await;

        // Client A creates and works in root context
        let client_a = connect_client(addr).await;
        let (kernel_a, kernel_id) = client_a.bind_kernel().await.unwrap();
        let root_ctx = kernel_a.create_context("shared-root").await.unwrap();
        kernel_a.join_context(root_ctx, "client-a").await.unwrap();

        // Client A runs a command
        let (_cmd_id, output, status) =
            shell_exec_wait(&kernel_a, "echo 'from client A'", root_ctx).await;
        assert_eq!(status, Status::Done, "client A echo failed: {output}");

        // Client B connects to same server
        let client_b = connect_client(addr).await;
        let (kernel_b, kernel_id_b) = client_b.bind_kernel().await.unwrap();
        assert_eq!(
            kernel_id, kernel_id_b,
            "both clients should see the same shared kernel"
        );

        // Client B can see the root context
        let contexts = kernel_b.list_contexts().await.unwrap();
        assert!(
            contexts.iter().any(|c| c.label == "shared-root"),
            "Client B should see 'shared-root' context"
        );

        // Client B joins and reads blocks
        kernel_b.join_context(root_ctx, "client-b").await.unwrap();
        let blocks = get_all_blocks(&kernel_b, root_ctx).await;
        let has_client_a_output = blocks.iter().any(|b| b.content.contains("from client A"));
        assert!(
            has_client_a_output,
            "Client B should see Client A's blocks, got: {:?}",
            blocks.iter().map(|b| &b.content).collect::<Vec<_>>()
        );
    });
}

// ============================================================================
// Variant: context creation and listing
// ============================================================================

#[test]
fn test_context_list_e2e() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;

        let (kernel, _) = client.bind_kernel().await.unwrap();

        // Create several contexts
        let ctx_a = kernel.create_context("ctx-alpha").await.unwrap();
        let _ctx_b = kernel.create_context("ctx-beta").await.unwrap();
        let _ctx_c = kernel.create_context("ctx-gamma").await.unwrap();
        kernel.join_context(ctx_a, "test").await.unwrap();

        // List via kj
        let (_cmd_id, list_output, list_status) =
            shell_exec_wait(&kernel, "kj context list", ctx_a).await;
        assert_eq!(
            list_status,
            Status::Done,
            "kj context list failed: {list_output}"
        );
        assert!(
            list_output.contains("ctx-alpha"),
            "should see ctx-alpha: {list_output}"
        );
        assert!(
            list_output.contains("ctx-beta"),
            "should see ctx-beta: {list_output}"
        );
        assert!(
            list_output.contains("ctx-gamma"),
            "should see ctx-gamma: {list_output}"
        );
    });
}

// ============================================================================
// Variant: shell command basics through RPC
// ============================================================================

#[test]
fn test_shell_echo_e2e() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("shell-test").await.unwrap();
        kernel.join_context(ctx, "test").await.unwrap();

        // Basic echo
        let (cmd_id, output, status) = shell_exec_wait(&kernel, "echo hello world", ctx).await;
        assert_eq!(status, Status::Done, "echo failed: {output}");
        assert!(
            output.contains("hello world"),
            "expected 'hello world', got: {output}"
        );

        // Verify block structure
        let blocks = get_all_blocks(&kernel, ctx).await;

        // Should have: ToolCall (command) + ToolResult (output)
        let tool_call = blocks
            .iter()
            .find(|b| b.id == cmd_id)
            .expect("command block not found");
        assert_eq!(tool_call.kind, BlockKind::ToolCall);

        let tool_result = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_id))
            .expect("output block not found");
        assert_eq!(tool_result.status, Status::Done);
    });
}

// ============================================================================
// Exit code propagation (gates structured shell return)
// ============================================================================

/// `shell_execute` must persist the kaish exit code on the ToolResult block.
/// Today only `Status::{Done, Error}` is set — `result.code` is dropped.
/// The MCP `context_shell` work depends on this so agents can distinguish
/// `kj` success/failure and shell command exit codes structurally rather than
/// by text-matching block content.
#[test]
fn test_shell_propagates_exit_code() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("exit-code-test").await.unwrap();
        kernel.join_context(ctx, "test").await.unwrap();

        // Success: `true` builtin → exit 0
        let (cmd_ok, _, status_ok) = shell_exec_wait(&kernel, "true", ctx).await;
        assert_eq!(status_ok, Status::Done, "`true` should succeed");
        let blocks = get_all_blocks(&kernel, ctx).await;
        let result_ok = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_ok))
            .expect("ToolResult for `true` not found");
        assert_eq!(
            result_ok.exit_code,
            Some(0),
            "`true` should populate exit_code=Some(0), got {:?}",
            result_ok.exit_code
        );

        // Failure: `false` builtin → exit 1
        let (cmd_err, _, status_err) = shell_exec_wait(&kernel, "false", ctx).await;
        assert_eq!(status_err, Status::Error, "`false` should fail");
        let blocks = get_all_blocks(&kernel, ctx).await;
        let result_err = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_err))
            .expect("ToolResult for `false` not found");
        assert_eq!(
            result_err.exit_code,
            Some(1),
            "`false` should populate exit_code=Some(1), got {:?}",
            result_err.exit_code
        );

        // kj help: success path through the kj builtin → exit 0
        let (cmd_kj, _, status_kj) = shell_exec_wait(&kernel, "kj help", ctx).await;
        assert_eq!(status_kj, Status::Done, "`kj help` should succeed");
        let blocks = get_all_blocks(&kernel, ctx).await;
        let result_kj = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_kj))
            .expect("ToolResult for `kj help` not found");
        assert_eq!(
            result_kj.exit_code,
            Some(0),
            "`kj help` should populate exit_code=Some(0), got {:?}",
            result_kj.exit_code
        );
    });
}

/// Regression pin for the truncation exit-code bug: `shell_execute` runs its
/// shell at `OutputProfile::Agent` (an 8 KB captured-output cap). When a
/// command's output crosses that cap, kaish-kernel truncates it AND remaps
/// the exit code to 3 (`did_spill`), stashing the command's real code in
/// `original_code` — a deliberate, loud signal aimed at a script's own `$?`.
/// `execute_shell_command` was persisting the remapped `3` onto the durable
/// ToolResult block regardless of what the command actually did, so a command
/// that ran to completion and exited 0 — merely printing more than 8 KB —
/// recorded `exit_code = 3` forever. `seq 1 5000` is a kaish builtin (no host
/// exec needed) that prints ~19 KB and always exits 0, so it deterministically
/// crosses the cap on every host, unlike the earlier `mount`-based flake this
/// bug hid behind (see `kj::context_shell::tests::
/// unknown_command_fails_fast_exec_granted_shell`, whose host-dependent
/// `mount` output was this same remap wearing a different command).
///
/// This test failed before the `original_code` resolution landed in
/// `execute_shell_command` (recorded `exit_code = Some(3)`) and passes after.
#[test]
fn test_shell_truncation_does_not_corrupt_exit_code() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("truncation-exit-code-test").await.unwrap();
        kernel.join_context(ctx, "test").await.unwrap();

        // seq 1 5000 prints far more than the 8 KB agent cap and always
        // succeeds — the exact "succeeded but got capped" shape the bug hid.
        let (cmd_id, output, status) = shell_exec_wait(&kernel, "seq 1 5000", ctx).await;
        assert_eq!(
            status,
            Status::Done,
            "a spilled-but-successful command must still read Done, got: {output}"
        );
        assert!(
            output.contains("[output truncated"),
            "kaish's own truncation marker should be visible in the persisted \
             body so the cap stays discoverable even without a structured \
             field on this path, got: {output}"
        );

        let blocks = get_all_blocks(&kernel, ctx).await;
        let result = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_id))
            .expect("ToolResult for `seq 1 5000` not found");
        assert_eq!(
            result.exit_code,
            Some(0),
            "a command that exited 0 but spilled output must record \
             exit_code=Some(0) on its durable ToolResult, not kaish's \
             truncation-remapped 3, got {:?}",
            result.exit_code
        );
    });
}

/// A `cd` and an `export` in one shell command must persist to the context's
/// durable L1 state, so the *next* command — which runs in a freshly
/// materialized, single-use shell re-seeded from L1 — lands in the same cwd and
/// sees the same exported var. Before cwd/env write-back existed, the throwaway
/// shell dropped both changes and the second command re-seeded from the
/// original cwd with no var. This is the regression guard for that round-trip.
///
/// Uses `CARGO_MANIFEST_DIR` as the cd target: a real host directory (the cwd
/// restore path validates against the host FS) that differs from the default
/// landing cwd (`$HOME`).
#[test]
fn test_shell_cd_and_export_persist_across_commands() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("cwd-persist-test").await.unwrap();
        kernel.join_context(ctx, "test").await.unwrap();

        let target = env!("CARGO_MANIFEST_DIR");

        // Baseline: capture the starting cwd so we can prove it actually moves.
        let (_, before, before_status) = shell_exec_wait(&kernel, "pwd", ctx).await;
        assert_eq!(before_status, Status::Done, "baseline pwd failed: {before}");
        assert!(
            !before.trim().is_empty(),
            "baseline pwd produced no output"
        );
        assert_ne!(
            before.trim(),
            target,
            "test precondition: starting cwd must differ from the cd target"
        );

        // Command 1 (one materialized shell): change dir and export a var.
        let (_, mutate_out, mutate_status) = shell_exec_wait(
            &kernel,
            &format!("cd {target}; export KJ_PERSIST_TEST=marker_value"),
            ctx,
        )
        .await;
        assert_eq!(
            mutate_status,
            Status::Done,
            "cd + export command failed: {mutate_out}"
        );

        // Command 2 (a *different* materialized shell, re-seeded from L1):
        // both the cwd and the exported var must have survived.
        let (_, after, after_status) =
            shell_exec_wait(&kernel, "pwd; echo \"marker=$KJ_PERSIST_TEST\"", ctx).await;
        assert_eq!(after_status, Status::Done, "follow-up command failed: {after}");
        assert!(
            after.contains(target),
            "cwd did not persist across commands: expected pwd to be {target}, got: {after}"
        );
        assert!(
            after.contains("marker=marker_value"),
            "exported var did not persist across commands, got: {after}"
        );
    });
}

/// `kj fork --prompt` should drive an autonomous turn in the child: the fork
/// publishes `turn.requested`, the server's turn driver consumes it and runs
/// `spawn_llm_for_prompt` for the child, and the mock provider streams a
/// response. We assert a Done assistant block appears in the *child* — it can
/// only exist if that whole chain fired. The parent is untouched (POSIX fork).
#[test]
fn test_fork_with_prompt_drives_autonomous_turn() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let main_ctx = kernel.create_context("main").await.unwrap();
        let _joined = kernel.join_context(main_ctx, "test").await.unwrap();

        // Fork with a seed. POSIX-style: this returns immediately on the parent;
        // the child starts acting on the seed via the turn driver.
        let (_id, out, status) = shell_exec_wait(
            &kernel,
            r#"kj fork --name explorer --prompt "investigate the bug""#,
            main_ctx,
        )
        .await;
        assert_eq!(status, Status::Done, "kj fork --prompt failed: {out}");

        // Locate the child and join so we can read its blocks.
        let contexts = kernel.list_contexts().await.unwrap();
        let child_id = contexts
            .iter()
            .find(|c| c.label == "explorer")
            .expect("explorer context not found in list")
            .id;
        let _joined = kernel.join_context(child_id, "test").await.unwrap();

        // Poll the child for a Done assistant block. Its presence proves the
        // autonomous turn ran end-to-end.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if std::time::Instant::now() > deadline {
                let blocks = get_all_blocks(&kernel, child_id).await;
                panic!(
                    "no assistant block appeared in child within 10s — the \
                     autonomous turn was not driven.\nchild blocks: {blocks:#?}"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let blocks = get_all_blocks(&kernel, child_id).await;
            let drove = blocks.iter().any(|b| {
                b.role == Role::Model && b.kind == BlockKind::Text && b.status == Status::Done
            });
            if drove {
                break;
            }
        }

        // The parent should NOT have been driven — no seed there, and fork
        // didn't switch us. (A Model block in main would mean cross-talk.)
        let main_blocks = get_all_blocks(&kernel, main_ctx).await;
        assert!(
            !main_blocks
                .iter()
                .any(|b| b.role == Role::Model && b.kind == BlockKind::Text),
            "parent context should not have taken a turn"
        );
    });
}

// ============================================================================
// rc create-lifecycle runs on the RPC creation path (app / MCP), not just
// `kj context create`. Regression guard for the divergent-creation-path bug:
// register_session / the GUI create dialog go through the kernel RPC
// `create_context`, which used to skip rc entirely.
// ============================================================================

#[test]
fn test_rpc_created_context_runs_rc_create() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // Create a context with the "coder" mode bundle over the RPC path —
        // the same path the GUI app and MCP facade take. Its rc create
        // lifecycle (`/etc/rc/coder/create/S00-stance.kai`) emits the coder
        // stance as a System/Text block via `kj block create`.
        let ctx = kernel
            .create_context_typed("rc-coder", "coder")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;
        let has_stance = blocks.iter().any(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && b.content.contains("You are coding")
        });
        assert!(
            has_stance,
            "expected coder stance block from rc create lifecycle on an \
             RPC-created context; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );
    });
}

/// The assistant seat's create lifecycle, same shape as the coder one above.
///
/// Worth having beyond "it emits a block": `assistant` is a brand-new rc
/// bucket, and nothing registers context types anywhere — a bucket is just a
/// directory name, and model resolution falls through when no cast claims the
/// role. So the only thing standing between "the seat works" and "the seat
/// silently does nothing" is that the directory is spelled right and seeded.
/// This is the test that fails if the bucket is renamed or dropped from the
/// embedded defaults.
#[test]
fn test_rpc_created_assistant_context_runs_its_stance() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let ctx = kernel
            .create_context_typed("rc-assistant", "assistant")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;
        let has_stance = blocks.iter().any(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && b.content.contains("the fleet's assistant seat")
        });
        assert!(
            has_stance,
            "expected assistant stance block from the rc create lifecycle; \
             got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );

        // The coder stance must not leak into a different bucket — the same
        // negative the default-context test makes, in the direction a
        // copy-pasted bucket would actually break.
        assert!(
            !blocks.iter().any(|b| b.content.contains("You are coding here")),
            "assistant context must not get the coder stance"
        );
    });
}

#[test]
fn test_rpc_default_context_type_is_default() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // Plain create_context (empty context_type on the wire) must still
        // land as "default" — no coder stance leaks in.
        let ctx = kernel.create_context("plain").await.unwrap();
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;
        // "You are coding inside kaijutsu" was the coder stance's opening
        // line at some point but the wording moved on (now "You are coding
        // here...", S00-stance.kai) and this negative assertion went
        // vacuous — it passed regardless of whether the coder stance leaked
        // in, since that exact string appears nowhere in the repo anymore.
        // Assert on the phrase the coder stance ACTUALLY opens with today so
        // a real leak trips this.
        assert!(
            !blocks.iter().any(|b| b.content.contains("You are coding here")),
            "default context must not get the coder stance; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );
    });
}

// ============================================================================
// Coder stance branch selection (S00-stance.kai) on the RPC creation path.
//
// NOTE what these two tests do and don't pin: `create_context_typed` goes
// through the kernel RPC `create_context`. Through 2026-08-10 that path
// STAMPED the registry-default provider/model onto the new `ContextRow`
// itself (`crates/kaijutsu-server/src/rpc.rs` `create_context_inner`)
// whenever no per-context override was given — a divergence from the kj
// dispatch path below, closed per `docs/issues.md` "Two context-creation
// paths disagree about stamping the model": neither path stamps now, so
// `.model` is genuinely null here too and `.resolved_model` is the only
// thing reading through to the registry default. These two tests still pin
// something real (branch selection follows the effective model when driven
// over RPC) but no longer distinguish `.model` from `.resolved_model` reads
// by themselves — see `test_coder_stance_crisp_for_null_row_model_via_kj_dispatch`
// below for the test that pins the null-row-model case explicitly (now true
// of both creation paths, not just kj dispatch).
// ============================================================================

#[test]
fn test_coder_stance_crisp_for_rpc_created_fast_model() {
    run_local(async {
        // Registry default only — no per-context model override.
        let addr = start_server_with_mock_llm_model("claude-haiku-4-5").await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let ctx = kernel
            .create_context_typed("rc-coder-crisp", "coder")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;

        // Trace block: precise signal of which branch fired and on what
        // model read — the rc script echoes this specifically so a
        // mis-routed branch is visible without a bisect.
        let has_crisp_trace = blocks.iter().any(|b| {
            b.kind == BlockKind::Trace
                && b.content.contains("stance: crisp branch")
                && b.content.contains("resolved_model=claude-haiku-4-5")
        });
        assert!(
            has_crisp_trace,
            "expected a 'stance: crisp branch (resolved_model=claude-haiku-4-5)' \
             trace block for a context inheriting a fast-executor model from \
             the registry default; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );

        // Stance text: "not a survey" is the crisp branch's defining
        // instruction (act, don't reconnoiter) and appears in no other
        // branch.
        let has_crisp_stance = blocks.iter().any(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && b.content.contains("not a survey")
        });
        assert!(
            has_crisp_stance,
            "expected the crisp coder stance (\"not a survey\") for a context \
             inheriting a fast-executor model; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );
    });
}

#[test]
fn test_coder_stance_synth_for_rpc_created_non_matching_model() {
    run_local(async {
        let addr = start_server_with_mock_llm_model("kaijutsu-reflective-test-model").await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let ctx = kernel
            .create_context_typed("rc-coder-synth", "coder")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;

        let has_synth_trace = blocks.iter().any(|b| {
            b.kind == BlockKind::Trace
                && b.content.contains("stance: synth branch")
                && b.content
                    .contains("resolved_model=kaijutsu-reflective-test-model")
        });
        assert!(
            has_synth_trace,
            "expected a 'stance: synth branch \
             (resolved_model=kaijutsu-reflective-test-model)' trace block for \
             a context inheriting a non-matching model from the registry \
             default; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );

        // Stance text: "one hand in a cybernetic loop" is the synth branch's
        // defining trait (equals in the loop, room to reflect) and appears
        // in no other branch.
        let has_synth_stance = blocks.iter().any(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && b.content.contains("one hand in a cybernetic loop")
        });
        assert!(
            has_synth_stance,
            "expected the synth coder stance (\"one hand in a cybernetic \
             loop\") for a context inheriting a non-matching model; got {} \
             blocks: {:#?}",
            blocks.len(),
            blocks
        );
    });
}

// ============================================================================
// The `.model`-vs-`.resolved_model` regression itself: a context whose
// `ContextRow.model` column is genuinely NULL. Since 2026-08-10 this is true
// of BOTH creation paths (see the note above the RPC tests), but this test
// pins it via the kj dispatch path specifically, which has always worked
// this way and is where the regression was first found.
//
// `kj context create <label> --type coder` — the kaish/kj dispatch path
// (`crates/kaijutsu-kernel/src/kj/context.rs`, `context_create`) — writes
// `model: None` on the row and only touches it if an explicit `--model` was
// given (`apply_context_config`). No `--model` here, so the row stays null
// and the rc create-lifecycle's `kj context info --json | jq -r '.model'`
// reads `null` while `.resolved_model` reads through to the registry
// default. This is the case the old buggy script silently sent down the
// synth branch no matter what model was actually bound (every ACP session,
// every default-resolved coder context). Driven through `shell_execute` so
// it exercises kj dispatch rather than the RPC `create_context` path.
// ============================================================================

#[test]
fn test_coder_stance_crisp_for_null_row_model_via_kj_dispatch() {
    run_local(async {
        // Registry default only; kj-dispatch context creation never stamps
        // it onto the row absent an explicit --model.
        let addr = start_server_with_mock_llm_model("claude-haiku-4-5").await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // A bootstrap context to run the `kj` shell command from — its own
        // type is irrelevant, it's just where the command executes.
        let boot_ctx = kernel.create_context("boot-kj-dispatch").await.unwrap();
        let _ = kernel.join_context(boot_ctx, "test").await.unwrap();

        let (_, create_output, create_status) = shell_exec_wait(
            &kernel,
            "kj context create rc-coder-kjdispatch --type coder",
            boot_ctx,
        )
        .await;
        assert_eq!(
            create_status,
            Status::Done,
            "kj context create failed: {create_output}"
        );

        let info = kernel
            .resolve_context_label("rc-coder-kjdispatch")
            .await
            .unwrap()
            .expect("rc-coder-kjdispatch should resolve after kj context create");
        let ctx = info.id;
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;

        let has_crisp_trace = blocks.iter().any(|b| {
            b.kind == BlockKind::Trace
                && b.content.contains("stance: crisp branch")
                && b.content.contains("resolved_model=claude-haiku-4-5")
        });
        assert!(
            has_crisp_trace,
            "expected a 'stance: crisp branch (resolved_model=claude-haiku-4-5)' \
             trace block for a kj-dispatch-created context with a null row \
             model and a fast-executor registry default; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );

        let has_crisp_stance = blocks.iter().any(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && b.content.contains("not a survey")
        });
        assert!(
            has_crisp_stance,
            "expected the crisp coder stance (\"not a survey\") for a \
             kj-dispatch-created context with a null row model and a \
             fast-executor registry default; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );
    });
}

// ============================================================================
// Regression for `docs/issues.md` "Two context-creation paths disagree about
// stamping the model" (2026-08-07). Through 2026-08-10, `create_context_inner`
// (the RPC path both `create_context_typed` and `create_context` share) read
// the registry default and wrote it onto the new `ContextRow`'s
// `provider`/`model` columns unconditionally — freezing a snapshot of
// whatever the default happened to be at creation time, and reporting
// `resolved_source: "context"` (as if an explicit override had been given)
// even though no caller asked for one. `kj context create` never did this:
// its row stays `provider: None, model: None` absent an explicit `--model`,
// so `resolve_context_model` falls through live to the registry default
// every call (`resolved_source: "default"`) and a later default change
// reaches it. The decision made closing this entry: neither path stamps —
// the row is the explicit-override slot only, never a creation-time cache of
// the default. This test pins that an RPC-created context, with a registry
// default configured but no explicit model given, has a null row and
// resolves via "default", matching what `kj context create` has always done.
// ============================================================================

#[test]
fn test_rpc_created_context_does_not_stamp_the_registry_default() {
    run_local(async {
        let addr = start_server_with_mock_llm_model("claude-haiku-4-5").await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let ctx = kernel
            .create_context_typed("rpc-unstamped", "default")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let (_, output, status) =
            shell_exec_wait(&kernel, "kj context info --json", ctx).await;
        assert_eq!(status, Status::Done, "kj context info --json failed: {output}");

        let data: serde_json::Value = serde_json::from_str(&output)
            .unwrap_or_else(|e| panic!("kj context info --json did not emit JSON ({e}): {output}"));

        assert!(
            data["provider"].is_null(),
            "RPC-created row.provider must be null (never stamped with the \
             registry default) — matching `kj context create`: {data}"
        );
        assert!(
            data["model"].is_null(),
            "RPC-created row.model must be null (never stamped with the \
             registry default) — matching `kj context create`: {data}"
        );
        assert_eq!(
            data["resolved_model"].as_str(),
            Some("claude-haiku-4-5"),
            "resolution must still reach the registry default live, through \
             `resolve_context_model`, even with a null row: {data}"
        );
        assert_eq!(
            data["resolved_source"].as_str(),
            Some("default"),
            "resolved_source must read \"default\" (live registry fallback), \
             not \"context\" (which implies an explicit override that was \
             never given): {data}"
        );
    });
}
