//! End-to-end integration tests for the fork → work → drift → merge workflow.
//!
//! Exercises the full SSH + Cap'n Proto stack with a mock LLM provider.
//! Each test starts a fresh ephemeral server + client.

mod common;
use common::*;

use kaijutsu_client::KernelHandle;
use kaijutsu_kernel::kernel_db::CharacterRow;
use kaijutsu_types::{BlockId, BlockKind, BlockQuery, BlockSnapshot, ContextId, PrincipalId, Role, Status};

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
        let main_ctx = create_context(&kernel, "main").await.unwrap();
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
        let alpha_id = create_context(&kernel, "alpha").await.unwrap();
        let beta_id = create_context(&kernel, "beta").await.unwrap();
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
        let root_ctx = create_context(&kernel_a, "shared-root").await.unwrap();
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
        let ctx_a = create_context(&kernel, "ctx-alpha").await.unwrap();
        let _ctx_b = create_context(&kernel, "ctx-beta").await.unwrap();
        let _ctx_c = create_context(&kernel, "ctx-gamma").await.unwrap();
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
        let ctx = create_context(&kernel, "shell-test").await.unwrap();
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
        let ctx = create_context(&kernel, "exit-code-test").await.unwrap();
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

/// A model-facing shell's output past the preview limit keeps the command's
/// exit. `seq 1 5000` is a kaish builtin (no host exec) that prints ~19 KB
/// and always exits 0, so it crosses the limit on every host. The durable
/// ToolResult must record exit 0 and carry the preview's
/// `[output truncated: ...]` line, which names the `/v/cas` path of the rest.
#[test]
fn test_shell_truncation_does_not_corrupt_exit_code() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = create_context(&kernel, "truncation-exit-code-test").await.unwrap();
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
        let ctx = create_context(&kernel, "cwd-persist-test").await.unwrap();
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
/// admits the request to the kernel runtime, which runs
/// `spawn_llm_for_prompt` for the child, and the mock provider streams a
/// response. We assert a Done assistant block appears in the *child* — it can
/// only exist if that whole chain fired. The parent is untouched (POSIX fork).
#[test]
fn test_fork_with_prompt_drives_autonomous_turn() {
    run_local(async {
        let (addr, shared_kernel) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let main_ctx = create_context(&kernel, "main").await.unwrap();
        let _joined = kernel.join_context(main_ctx, "test").await.unwrap();
        let coder = PrincipalId::new();
        let lead = PrincipalId::new();
        {
            let db = shared_kernel.kernel_db.lock();
            for (principal_id, name) in [(coder, "coder"), (lead, "lead")] {
                db.insert_character(&CharacterRow {
                    principal_id,
                    name: name.into(),
                    created_at: 0,
                    retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                })
                .unwrap();
            }
            db.update_context_review(main_ctx, Some(coder), Some(lead))
                .unwrap();
        }

        // Fork with a seed. POSIX-style: this returns immediately on the parent;
        // the child starts acting on the seed via the kernel runtime.
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
        // lifecycle (`/config/rc/coder/create/S00-stance.kai`) emits the coder
        // stance as a System/Text block via `kj block create`.
        let ctx = create_context_typed(&kernel, "rc-coder", "coder")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;
        let has_stance = blocks.iter().any(|b| {
            b.role == Role::System
                && b.kind == BlockKind::Text
                && b.content.contains("You are a coder.")
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

        let ctx = create_context_typed(&kernel, "rc-assistant", "assistant")
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
            !blocks.iter().any(|b| b.content.contains("You are a coder.")),
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
        let ctx = create_context(&kernel, "plain").await.unwrap();
        let _ = kernel.join_context(ctx, "test").await.unwrap();

        let blocks = get_all_blocks(&kernel, ctx).await;
        // Assert on the phrase the coder stance opens with today
        // (coder/create/S00-stance.md), so a real leak trips this; a stale
        // phrase would pass regardless.
        assert!(
            !blocks.iter().any(|b| b.content.contains("You are a coder.")),
            "default context must not get the coder stance; got {} blocks: {:#?}",
            blocks.len(),
            blocks
        );
    });
}

// ============================================================================
// One coder stance for every model (coder/create/S00-stance.md).
// ============================================================================

/// The coder stance is written in one register for every model (AGENTS.md,
/// "Write stances in kaibo's register"). A frontier model and a fast model get
/// the same text, and no stance branches on the model name.
#[test]
fn test_coder_stance_is_the_same_for_every_model() {
    let stance_for = |model: &'static str| async move {
        let addr = start_server_with_mock_llm_model(model).await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
        let ctx = create_context_typed(&kernel, "rc-coder-stance", "coder")
            .await
            .expect("create_context_typed");
        let _ = kernel.join_context(ctx, "test").await.unwrap();
        let blocks = get_all_blocks(&kernel, ctx).await;
        assert!(
            !blocks.iter().any(|b| b.kind == BlockKind::Trace && b.content.contains("tier")),
            "no stance tier is chosen for {model}: {blocks:#?}"
        );
        blocks
            .into_iter()
            .find(|b| b.role == Role::System && b.kind == BlockKind::Text && b.content.starts_with("You are a coder."))
            .unwrap_or_else(|| panic!("no coder stance for {model}"))
            .content
    };
    run_local(async move {
        let frontier = stance_for("claude-opus-4-6").await;
        let fast = stance_for("claude-haiku-4-5").await;
        assert_eq!(frontier, fast, "one stance for every model");
    });
}

#[test]
fn test_rpc_created_context_does_not_stamp_the_registry_default() {
    run_local(async {
        let addr = start_server_with_mock_llm_model("claude-haiku-4-5").await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let ctx = create_context_typed(&kernel, "rpc-unstamped", "default")
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

// ============================================================================
// Variant: quiet executeKj authors no blocks
// ============================================================================

#[test]
fn test_execute_kj_quiet_authors_no_blocks_e2e() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let ctx = create_context(&kernel, "quiet-kj").await.unwrap();
        kernel.join_context(ctx, "test").await.unwrap();

        let argv = vec!["ledger".to_string(), "list".to_string()];

        let before = get_all_blocks(&kernel, ctx).await.len();
        let quiet = kernel
            .execute_kj_quiet(ctx, &argv)
            .await
            .expect("quiet execute_kj");
        assert_eq!(quiet.exit_code, 0, "kj ledger list should succeed: {}", quiet.stderr);
        assert!(
            quiet.command_block_id.is_none(),
            "a quiet run authors no block, so there is no command_block_id to report"
        );
        let after_quiet = get_all_blocks(&kernel, ctx).await.len();
        assert_eq!(
            before, after_quiet,
            "a quiet executeKj must not author a tool-call/tool-result pair"
        );

        let loud = kernel.execute_kj(ctx, &argv).await.expect("non-quiet execute_kj");
        assert_eq!(loud.exit_code, 0, "kj ledger list should succeed: {}", loud.stderr);
        assert!(
            loud.command_block_id.is_some(),
            "a non-quiet run authors a block and must report its id"
        );
        let after_loud = get_all_blocks(&kernel, ctx).await.len();
        assert_eq!(
            after_loud,
            after_quiet + 2,
            "a non-quiet executeKj must author exactly a tool-call/tool-result pair"
        );
    });
}
