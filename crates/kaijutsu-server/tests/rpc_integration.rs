//! Integration tests for kaijutsu RPC over SSH
//!
//! Uses ephemeral SSH keys generated in memory for testing.

mod common;
use common::*;

#[test]
fn test_whoami() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let identity = client.whoami().await.unwrap();
        assert_eq!(identity.username, "test_user");
        assert_eq!(identity.display_name, "test_user");
    });
}

#[test]
fn test_list_kernels_shows_shared_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // The shared kernel is created at server startup — always one kernel
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1, "Expected one shared kernel");
    });
}

#[test]
fn test_attach_kernel_creates_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel (server auto-creates)
        let (kernel, kernel_id) = client.attach_kernel().await.unwrap();
        let info = kernel.get_info().await.unwrap();
        assert!(!kernel_id.is_nil());
        assert_eq!(info.id, kernel_id);
    });
}

#[test]
fn test_kernel_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel
        let (_kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Check it appears in list
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1);
    });
}

#[test]
fn test_create_context_returns_valid_id() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let context_id = kernel.create_context("test-ctx").await.unwrap();

        assert!(
            !context_id.is_nil(),
            "createContext should return a non-nil ContextId"
        );
    });
}

#[test]
fn test_create_context_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Count contexts before
        let before = kernel.list_contexts().await.unwrap();
        let before_count = before.len();

        let context_id = kernel.create_context("my-label").await.unwrap();

        // Should appear in list with correct label
        let after = kernel.list_contexts().await.unwrap();
        assert_eq!(
            after.len(),
            before_count + 1,
            "New context should appear in list"
        );

        let found = after.iter().find(|c| c.id == context_id);
        assert!(found.is_some(), "Created context should be findable by ID");
        assert_eq!(found.unwrap().label, "my-label");
    });
}

#[test]
fn test_create_context_joinable() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let context_id = kernel.create_context("joinable").await.unwrap();

        // Should be joinable
        let joined_id = kernel
            .join_context(context_id, "test-instance")
            .await
            .unwrap();
        assert_eq!(
            joined_id, context_id,
            "Joining a created context should return the same ID"
        );
    });
}

#[test]
fn test_create_context_invalid_label_is_hard_error() {
    // Regression for "ghost contexts" bug: KernelDb rejects labels containing ':'
    // via validate_label(). Before the fix, rpc.rs logged a warn and continued
    // to register the context in DriftRouter, leaving a ghost that was live in
    // memory but missing from the KernelDb (and thus lost on restart).
    //
    // The fix must (1) return Err from create_context when KernelDb insert fails
    // and (2) not leak the context into DriftRouter.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        let before = kernel.list_contexts().await.unwrap();
        let before_count = before.len();

        let result = kernel.create_context("bad:label").await;
        assert!(
            result.is_err(),
            "create_context with ':' in label must return Err (got {:?})",
            result
        );

        let after = kernel.list_contexts().await.unwrap();
        assert_eq!(
            after.len(),
            before_count,
            "no ghost context should be registered after a failed create"
        );
    });
}

#[test]
fn test_join_nonexistent_context_fails() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Joining a random context that was never created should fail
        let random_id = kaijutsu_crdt::ContextId::new();
        let result = kernel.join_context(random_id, "test-instance").await;
        assert!(
            result.is_err(),
            "join_context with nonexistent ID should fail"
        );
    });
}

#[test]
fn test_create_context_unique_ids() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let id1 = kernel.create_context("ctx-a").await.unwrap();
        let id2 = kernel.create_context("ctx-b").await.unwrap();

        assert_ne!(id1, id2, "Each created context should have a unique ID");
    });
}

// ============================================================================
// MCP Remote-Mode Tests (M6-G3)
// ============================================================================

#[test]
fn test_call_mcp_tool_dispatches_builtin_over_ssh() {
    // SSH-connected dispatch through call_mcp_tool. Exercises the full
    // wire: client → SSH channel → capnp → rpc.rs → broker → builtin
    // server → result back. Uses `whoami` (KernelInfoServer) — no LLM
    // configured, no external state, deterministic shape.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let ctx_id = kernel.create_context("mcp-remote-test").await.unwrap();
        kernel.join_context(ctx_id, "test-mcp").await.unwrap();

        let result = kernel
            .call_mcp_tool("builtin.kernel_info", "whoami", &serde_json::json!({}))
            .await
            .expect("call_mcp_tool over SSH");
        assert!(
            !result.is_error,
            "whoami should not be an error: content={}",
            result.content
        );
        assert!(
            !result.content.is_empty(),
            "whoami should return non-empty content"
        );
    });
}

#[test]
fn test_call_mcp_tool_unknown_tool_errors() {
    // Unknown tool name surfaces over the wire as an Err, not a silent
    // success. Locks in the error-propagation path through the SSH +
    // capnp + broker stack.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        let ctx_id = kernel.create_context("mcp-remote-error").await.unwrap();
        kernel.join_context(ctx_id, "test-mcp").await.unwrap();

        let result = kernel
            .call_mcp_tool("builtin.kernel_info", "no_such_tool", &serde_json::json!({}))
            .await;
        assert!(
            result.is_err(),
            "unknown tool should surface as Err over SSH, got: {result:?}"
        );
    });
}

#[test]
fn test_call_mcp_tool_requires_joined_context() {
    // Without a joined context, call_mcp_tool errors instead of falling
    // back to a default — the dispatch path needs context_id to resolve
    // the binding.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();
        // No join_context call.

        let result = kernel
            .call_mcp_tool("builtin.kernel_info", "whoami", &serde_json::json!({}))
            .await;
        assert!(
            result.is_err(),
            "call_mcp_tool without joined context should error, got: {result:?}"
        );
    });
}

// ============================================================================
// Async Execute Tests
// ============================================================================

/// Helper: attach kernel, create context, join it. Returns KernelHandle.
async fn setup_execute_context(
    addr: std::net::SocketAddr,
) -> (kaijutsu_client::RpcClient, kaijutsu_client::KernelHandle) {
    let client = connect_client(addr).await;
    let (kernel, _) = client.attach_kernel().await.unwrap();
    let ctx_id = kernel.create_context("exec-test").await.unwrap();
    kernel.join_context(ctx_id, "test-exec").await.unwrap();
    (client, kernel)
}

#[test]
fn test_execute_returns_immediately() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        // Subscribe to output first.
        let mut rx = kernel.subscribe_output().await.unwrap();

        // Execute a command that sleeps 2 seconds.
        let start = std::time::Instant::now();
        let exec_id = kernel.execute("sleep 2").await.unwrap();
        let elapsed = start.elapsed();

        // The RPC should return immediately (well under 2s).
        assert!(exec_id > 0, "exec_id should be positive");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "execute() should return immediately, took {:?}",
            elapsed
        );

        // Wait for exit code event to confirm completion.
        let exit_event = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_exit_code(&mut rx, exec_id),
        )
        .await
        .expect("timed out waiting for exit code");
        assert_eq!(exit_event, 0, "sleep 2 should exit with code 0");
    });
}

#[test]
fn test_execute_output_events() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let mut rx = kernel.subscribe_output().await.unwrap();

        // Run a command that produces stdout.
        let exec_id = kernel.execute("echo hello").await.unwrap();

        // Collect all events for this exec_id.
        let events = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            collect_output_events(&mut rx, exec_id),
        )
        .await
        .expect("timed out waiting for output events");

        let stdout: String = events
            .iter()
            .filter_map(|e| match e {
                kaijutsu_client::OutputEvent::Stdout { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            stdout.contains("hello"),
            "stdout should contain 'hello', got: {:?}",
            stdout
        );

        let exit_code = events
            .iter()
            .find_map(|e| match e {
                kaijutsu_client::OutputEvent::ExitCode { code, .. } => Some(*code),
                _ => None,
            })
            .expect("should have exit code event");
        assert_eq!(exit_code, 0);

        // Run a failing command.
        let exec_id2 = kernel.execute("false").await.unwrap();
        let events2 = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            collect_output_events(&mut rx, exec_id2),
        )
        .await
        .expect("timed out waiting for failing command events");

        let exit_code2 = events2
            .iter()
            .find_map(|e| match e {
                kaijutsu_client::OutputEvent::ExitCode { code, .. } => Some(*code),
                _ => None,
            })
            .expect("should have exit code event for failing command");
        assert_ne!(exit_code2, 0, "false should exit non-zero");
    });
}

#[test]
fn test_interrupt_cancels_execution() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let mut rx = kernel.subscribe_output().await.unwrap();

        // Start a long-running command.
        let exec_id = kernel.execute("sleep 60").await.unwrap();

        // Give it a moment to start, then interrupt.
        tokio::task::yield_now().await;
        kernel.interrupt(exec_id).await.unwrap();

        // Should get an exit code event (non-zero).
        let exit_code = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_exit_code(&mut rx, exec_id),
        )
        .await
        .expect("timed out waiting for interrupted exit code");
        assert_ne!(exit_code, 0, "interrupted command should exit non-zero");

        // Verify the kernel is not broken — next execute should work.
        let exec_id2 = kernel.execute("echo ok").await.unwrap();
        let exit_code2 = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_exit_code(&mut rx, exec_id2),
        )
        .await
        .expect("timed out waiting for post-interrupt command");
        assert_eq!(exit_code2, 0, "post-interrupt command should succeed");
    });
}

#[test]
fn test_concurrent_execute_rejected() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let mut rx = kernel.subscribe_output().await.unwrap();

        // Start a long-running command.
        let exec_id = kernel.execute("sleep 5").await.unwrap();

        // Yield to let the background task start.
        tokio::task::yield_now().await;

        // Second execute should fail while first is running.
        let result = kernel.execute("echo hi").await;
        assert!(
            result.is_err(),
            "concurrent execute should be rejected while one is running"
        );

        // Clean up: interrupt the first command.
        kernel.interrupt(exec_id).await.unwrap();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_exit_code(&mut rx, exec_id),
        )
        .await;
    });
}

// ============================================================================
// Test Helpers
// ============================================================================

/// Wait for an ExitCode event for the given exec_id, discarding other events.
async fn wait_for_exit_code(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<kaijutsu_client::OutputEvent>,
    exec_id: u64,
) -> i32 {
    loop {
        match rx.recv().await {
            Some(kaijutsu_client::OutputEvent::ExitCode { exec_id: id, code }) if id == exec_id => {
                return code;
            }
            Some(_) => continue,
            None => panic!("output channel closed before exit code received"),
        }
    }
}

/// Collect all output events for the given exec_id until ExitCode is received.
async fn collect_output_events(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<kaijutsu_client::OutputEvent>,
    exec_id: u64,
) -> Vec<kaijutsu_client::OutputEvent> {
    let mut events = Vec::new();
    loop {
        match rx.recv().await {
            Some(event) => {
                let is_exit = matches!(
                    &event,
                    kaijutsu_client::OutputEvent::ExitCode { exec_id: id, .. } if *id == exec_id
                );
                let matches_id = match &event {
                    kaijutsu_client::OutputEvent::Stdout { exec_id: id, .. } => *id == exec_id,
                    kaijutsu_client::OutputEvent::Stderr { exec_id: id, .. } => *id == exec_id,
                    kaijutsu_client::OutputEvent::ExitCode { exec_id: id, .. } => *id == exec_id,
                };
                if matches_id {
                    events.push(event);
                }
                if is_exit {
                    return events;
                }
            }
            None => panic!("output channel closed before exit code received"),
        }
    }
}
