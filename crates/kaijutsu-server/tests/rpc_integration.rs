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

/// The server dispatches the RPC handler by subsystem name, not by channel
/// ordinal. A channel that requests an unknown subsystem must never be bound to
/// RPC — so an RPC call over it must fail (or never be answered), never succeed.
/// This is the regression guard for the named-subsystem migration: if dispatch
/// silently fell back to "attach RPC to any channel", whoami would succeed here.
#[test]
fn test_unknown_subsystem_is_not_bound_to_rpc() {
    run_local(async {
        let addr = start_server().await;

        let config = kaijutsu_client::SshConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "test_user".to_string(),
            key_source: kaijutsu_client::KeySource::ephemeral(),
            insecure: true,
        };
        let mut ssh = kaijutsu_client::SshClient::new(config);

        // Auth succeeds and the channel opens, but the server must refuse to
        // bind an unknown subsystem — no Cap'n Proto handler is attached.
        let channel = ssh
            .connect_subsystem("definitely-not-a-real-subsystem")
            .await
            .expect("channel open + subsystem request should still send");
        let client = kaijutsu_client::RpcClient::new(channel.into_stream())
            .await
            .expect("rpc client init over the raw stream");

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.whoami()).await;

        match result {
            Ok(Ok(_)) => {
                panic!("whoami succeeded — server wrongly bound an unknown subsystem to RPC")
            }
            // Channel refused/closed → RPC fails fast. Correct.
            Ok(Err(_)) => {}
            // No handler ever answered → also correct: nothing was bound.
            Err(_elapsed) => {}
        }
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
fn test_bind_kernel_creates_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel (server auto-creates)
        let (kernel, kernel_id) = client.bind_kernel().await.unwrap();
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
        let (_kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // Check it appears in list
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1);
    });
}

/// get_config reads the CRDT-owned config over the wire (client → SSH → capnp →
/// rpc.rs → /etc/config VFS). A fresh kernel seeds the embedded defaults, so
/// theme.toml comes back non-empty; an unknown file is a loud error, not "".
#[test]
fn test_get_config_reads_crdt_owned_theme() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        // Seeded theme.toml round-trips (bare name resolves under /etc/config).
        let theme = kernel.get_config("theme.toml").await.unwrap();
        assert!(theme.contains("bg"), "seeded theme.toml should carry bg: {theme}");

        // Unknown config file surfaces an error rather than empty content.
        let err = kernel.get_config("nonesuch.toml").await;
        assert!(err.is_err(), "unknown config must error, got {err:?}");
    });
}

/// `Vfs.snapshot` round-trip over the real wire: client → SSH → capnp →
/// rpc.rs `VfsImpl::snapshot` → kernel `MountTable::snapshot` → recursive
/// capnp `SnapshotNode` reply → client's owned tree. `/etc/rc` is a
/// CRDT-native (virtual) mount seeded with lifecycle scripts at kernel boot,
/// so it's guaranteed non-empty without touching the host filesystem — and
/// it doubles as coverage that a virtual backend reports `ignored: false`
/// (no gitignore semantics off the real filesystem) over the wire, not just
/// in the kernel-side unit tests.
#[test]
fn test_vfs_snapshot_round_trips_over_rpc() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let result = kernel.vfs_snapshot("/etc/rc", 3, 500).await.unwrap();

        assert_eq!(result.root.name, "rc");
        assert!(matches!(
            result.root.kind,
            kaijutsu_client::VfsFileType::Directory
        ));
        assert!(
            !result.root.children.is_empty(),
            "seeded /etc/rc should have entries"
        );
        assert_eq!(result.generation, result.root.generation);
        // Virtual backend: no gitignore semantics apply anywhere in the tree.
        fn assert_never_ignored(node: &kaijutsu_client::SnapshotNode) {
            assert!(!node.ignored, "virtual backend node reported ignored: {}", node.name);
            for child in &node.children {
                assert_never_ignored(child);
            }
        }
        assert_never_ignored(&result.root);

        // A tiny cap forces a visible cut, proving truncated_here/truncated
        // survive the wire round-trip (not just the in-process walker).
        let cut = kernel.vfs_snapshot("/etc/rc", 3, 1).await.unwrap();
        assert!(cut.truncated, "max_entries=1 must truncate a populated tree");
        assert!(cut.root.truncated_here);
    });
}

#[test]
fn test_create_context_returns_valid_id() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

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

/// Stage 1 (time-well) kernel truth, server slice: `listContexts` must
/// populate `ContextHandleInfo.lastActivityAt` from the kernel DB's
/// `last_activity_at` column, alongside the existing `set_concluded_at` (both
/// come off the same `ContextRow`). Context creation itself already runs rc
/// lifecycle (e.g. the stance block), so a fresh context can already carry a
/// stamp — the invariant under test is that a *subsequent* block-mutating op
/// (here, `shell_execute`'s synchronous command-block insert, which goes
/// through `BlockStore::journal_op`) advances it to a fresh, recent value.
#[test]
fn test_context_last_activity_at_populated_after_block_op() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("activity-stamp").await.unwrap();
        kernel
            .join_context(context_id, "test-instance")
            .await
            .unwrap();

        let before = kernel.list_contexts().await.unwrap();
        let found_before = before
            .iter()
            .find(|c| c.id == context_id)
            .expect("just-created context must appear in list_contexts");
        // May already be `Some(_)` (rc create lifecycle inserts blocks too) —
        // the point of this test is the *advance* past t0, not a bare `None`.
        let before_stamp = found_before.last_activity_at;

        // Guarantee millis-resolution separation from `before_stamp`.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let t0 = kaijutsu_types::now_millis();
        kernel
            .shell_execute("echo activity-stamp-probe", context_id, true)
            .await
            .unwrap_or_else(|e| panic!("shell_execute failed: {e}"));

        let after = kernel.list_contexts().await.unwrap();
        let found_after = after
            .iter()
            .find(|c| c.id == context_id)
            .expect("context must still appear in list_contexts after the block op");
        let stamped = found_after.last_activity_at.expect(
            "a block-mutating op (shell_execute's command-block insert) must stamp \
             last_activity_at via journal_op -> touch_context_activity",
        );
        assert!(
            stamped >= t0,
            "stamp {stamped} should be >= t0 {t0} recorded just before the block op"
        );
        if let Some(before) = before_stamp {
            assert!(
                stamped > before,
                "stamp {stamped} should advance past the pre-op stamp {before}"
            );
        }
    });
}

#[test]
fn test_create_context_joinable() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
        let ctx_id = kernel.create_context("mcp-remote-test").await.unwrap();
        kernel.join_context(ctx_id, "test-mcp").await.unwrap();

        let result = kernel
            .call_mcp_tool("whoami", &serde_json::json!({}))
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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
        let ctx_id = kernel.create_context("mcp-remote-error").await.unwrap();
        kernel.join_context(ctx_id, "test-mcp").await.unwrap();

        let result = kernel
            .call_mcp_tool("no_such_tool", &serde_json::json!({}))
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

        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();
        // No join_context call.

        let result = kernel
            .call_mcp_tool("whoami", &serde_json::json!({}))
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
    let (kernel, _) = client.bind_kernel().await.unwrap();
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

/// Regression: many short-lived connections rapidly create+join contexts.
///
/// Before parking_lot::RwLock<DriftRouter> the same lock was a tokio fair
/// RwLock. `kj/stage.rs` reached it via `blocking_read()` from inside an
/// async fn, which could deadlock a current_thread runtime if a writer was
/// queued. This test exercises concurrent writers (create_context) and a
/// reader (list_contexts) on the shared drift router across many SSH
/// connections in quick succession — it should not hang or wedge.
#[test]
fn test_drift_router_concurrent_access_does_not_wedge() {
    run_local(async {
        let addr = start_server().await;

        // Open three concurrent clients on the same kernel; each creates a
        // context then lists. The lock is shared across all of them.
        let make = |label: &'static str| async move {
            let client = connect_client(addr).await;
            let (kernel, _) = client.bind_kernel().await.unwrap();
            let ctx = kernel.create_context(label).await.unwrap();
            let listed = kernel.list_contexts().await.unwrap();
            assert!(
                listed.iter().any(|c| c.id == ctx),
                "context just created should appear in list",
            );
            ctx
        };

        let (a, b, c) = tokio::join!(make("a"), make("b"), make("c"));
        // Cheap sanity: distinct ids.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    });
}

/// Regression: a client that subscribes to block events then drops cleanly
/// must not wedge the server's bridge task. The bridge observes the
/// connection cancellation token and exits its select loop. Before this
/// change the bridge only exited when a callback `req.send().promise.await`
/// returned Err, which can take arbitrarily long if the peer half-closed
/// without sending FIN.
#[test]
fn test_subscribe_blocks_filtered_cleans_up_on_client_drop() {
    run_local(async {
        let addr = start_server().await;
        {
            let client = connect_client(addr).await;
            let (kernel, _) = client.bind_kernel().await.unwrap();
            let ctx = kernel.create_context("subscribe-cleanup").await.unwrap();
            kernel.join_context(ctx, "drop-test").await.unwrap();
            // Subscribe with no events expected — just register the callback,
            // then let `client` drop at end of scope.
            // (The actor crate sets up subscriptions internally via setup_subscriptions.)
            let _ = kernel.list_contexts().await.unwrap();
        } // client + kernel + connection dropped here

        // Open a fresh client. If the previous bridge task wedged on the
        // shared kernel/lock, this would hang. Bound the entire round-trip
        // with a short timeout so a regression surfaces as a test failure
        // rather than a hang.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let client = connect_client(addr).await;
            let (kernel, _) = client.bind_kernel().await.unwrap();
            kernel.list_contexts().await.unwrap()
        })
        .await;
        assert!(
            result.is_ok(),
            "post-drop reconnect must succeed within 5s — bridge cleanup wedged?",
        );
    });
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

// ─── Shell vars / cwd are durable, context-scoped L1 state ─────────────────
// After the per-connection kaish was retired, shell-var RPCs read/write the
// context's durable env (L1 `context_env`) rather than a connection-local
// shell. These tests lock that contract.

#[test]
fn test_shell_var_round_trips_through_context() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        kernel
            .set_shell_var(
                "GREETING",
                &kaijutsu_client::ShellValue::String("hello".into()),
            )
            .await
            .unwrap();

        let (value, found) = kernel.get_shell_var("GREETING").await.unwrap();
        assert!(found, "var should be found after set");
        assert_eq!(
            value,
            Some(kaijutsu_client::ShellValue::String("hello".into()))
        );

        let vars = kernel.list_shell_vars().await.unwrap();
        assert!(
            vars.iter().any(|(k, v)| k == "GREETING"
                && *v == kaijutsu_client::ShellValue::String("hello".into())),
            "list_shell_vars should include the set var, got: {:?}",
            vars
        );
    });
}

/// Shell vars live in the context's durable L1 env, not on a per-connection
/// kaish. A var set by one connection MUST be visible to a second connection
/// joined to the same context. This would fail under the old per-connection
/// model where each connection held its own transient shell scope.
#[test]
fn test_shell_var_shared_across_connections() {
    run_local(async {
        let addr = start_server().await;

        // Connection A creates + joins a context and sets a var.
        let client_a = connect_client(addr).await;
        let (kernel_a, _) = client_a.bind_kernel().await.unwrap();
        let ctx_id = kernel_a.create_context("shared-env").await.unwrap();
        kernel_a.join_context(ctx_id, "conn-a").await.unwrap();
        kernel_a
            .set_shell_var(
                "SHARED",
                &kaijutsu_client::ShellValue::String("from-a".into()),
            )
            .await
            .unwrap();

        // Connection B joins the SAME context and reads the var back.
        let client_b = connect_client(addr).await;
        let (kernel_b, _) = client_b.bind_kernel().await.unwrap();
        kernel_b.join_context(ctx_id, "conn-b").await.unwrap();

        let (value, found) = kernel_b.get_shell_var("SHARED").await.unwrap();
        assert!(
            found,
            "var set on connection A must be visible to connection B (durable L1)"
        );
        assert_eq!(
            value,
            Some(kaijutsu_client::ShellValue::String("from-a".into()))
        );
    });
}

/// A durable env var seeds every materialized shell, so `$VAR` expands inside an
/// interactive `execute`. Proves the L1 → materialized-shell seeding path that
/// replaced the cached per-connection kaish.
#[test]
fn test_shell_var_seeds_materialized_shell() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        kernel
            .set_shell_var(
                "SEEDED",
                &kaijutsu_client::ShellValue::String("visible".into()),
            )
            .await
            .unwrap();

        let mut rx = kernel.subscribe_output().await.unwrap();
        let exec_id = kernel.execute("echo $SEEDED").await.unwrap();
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
            stdout.contains("visible"),
            "materialized shell should expand the durable env var, got: {:?}",
            stdout
        );
    });
}

// ============================================================================
// Per-client durable view state (docs/shared-state.md "Retiring KV")
// ============================================================================

/// `setLastContext` / `getClientView` round-trip over the real wire — the
/// typed replacement for the app's one production KV use
/// (`<client-id>.current_context`).
#[test]
fn test_client_view_round_trips_over_rpc() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let client_id = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            kernel.get_client_view(&client_id).await.unwrap(),
            None,
            "no view recorded yet"
        );

        let ctx_id = kernel.create_context("client-view-a").await.unwrap();
        kernel.set_last_context(&client_id, ctx_id).await.unwrap();

        assert_eq!(
            kernel.get_client_view(&client_id).await.unwrap(),
            Some(ctx_id),
            "view survives write→read over the wire"
        );
    });
}

/// A second `setLastContext` for the same client overwrites the first — one
/// row per installation, not a history of every context ever viewed.
#[test]
fn test_client_view_set_twice_returns_latest() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let client_id = uuid::Uuid::new_v4().to_string();
        let ctx_a = kernel.create_context("client-view-b").await.unwrap();
        let ctx_b = kernel.create_context("client-view-c").await.unwrap();

        kernel.set_last_context(&client_id, ctx_a).await.unwrap();
        kernel.set_last_context(&client_id, ctx_b).await.unwrap();

        assert_eq!(
            kernel.get_client_view(&client_id).await.unwrap(),
            Some(ctx_b),
            "second set overwrites the first"
        );
    });
}

/// Two client ids don't clobber each other's view — the whole point of
/// keying by client_id instead of a single global row (mirrors
/// `test_shell_var_shared_across_connections`'s isolation shape, but across
/// distinct client ids on ONE connection rather than distinct connections).
#[test]
fn test_client_view_is_namespaced_per_client() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let client_a = uuid::Uuid::new_v4().to_string();
        let client_b = uuid::Uuid::new_v4().to_string();
        let ctx_a = kernel.create_context("client-view-d").await.unwrap();
        let ctx_b = kernel.create_context("client-view-e").await.unwrap();

        kernel.set_last_context(&client_a, ctx_a).await.unwrap();
        kernel.set_last_context(&client_b, ctx_b).await.unwrap();

        assert_eq!(kernel.get_client_view(&client_a).await.unwrap(), Some(ctx_a));
        assert_eq!(kernel.get_client_view(&client_b).await.unwrap(), Some(ctx_b));
    });
}

// ============================================================================
// Time-well ring placement (docs/timewell.md)
// ============================================================================

/// `promoteContext`/`demoteContext`/`setContextPaused` round-trip over the
/// real wire and `listContexts` reflects the ladder as it steps: promoted →
/// unpromoted → demoted → archived, with pause/resume independent of ring
/// placement.
#[test]
fn test_promote_demote_pause_round_trip_over_rpc() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let ctx = kernel.create_context("ring-rider").await.unwrap();

        let find = |contexts: &[kaijutsu_client::ContextInfo], id: kaijutsu_types::ContextId| {
            contexts.iter().find(|c| c.id == id).cloned()
        };

        kernel.promote_context(ctx).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = find(&contexts, ctx).expect("promoted context still listed");
        assert!(row.promoted_at.is_some());
        assert!(row.demoted_at.is_none());

        // promoted → unpromoted
        kernel.demote_context(ctx).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = find(&contexts, ctx).unwrap();
        assert!(row.promoted_at.is_none());
        assert!(row.demoted_at.is_none());
        assert!(!row.archived);

        // neither → demoted
        kernel.demote_context(ctx).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = find(&contexts, ctx).unwrap();
        assert!(row.demoted_at.is_some());
        assert!(!row.archived);

        // already demoted → archived
        kernel.demote_context(ctx).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = find(&contexts, ctx).unwrap();
        assert!(row.archived, "third demote step should archive the context");

        // A further demote is a loud RPC error, not a silent no-op.
        let err = kernel.demote_context(ctx).await.unwrap_err();
        assert!(matches!(err, kaijutsu_client::RpcError::ServerError(_)));

        // Promote is the resurrection door: promoting the archived context
        // unarchives it and seats it in ring 0.
        kernel.promote_context(ctx).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = find(&contexts, ctx).unwrap();
        assert!(!row.archived, "promote must resurrect an archived context");
        assert!(row.promoted_at.is_some());
        assert!(row.demoted_at.is_none());

        // Pause/resume are independent of ring placement.
        let ctx2 = kernel.create_context("napper").await.unwrap();
        kernel.set_context_paused(ctx2, true).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        assert!(find(&contexts, ctx2).unwrap().paused_at.is_some());
        kernel.set_context_paused(ctx2, false).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        assert!(find(&contexts, ctx2).unwrap().paused_at.is_none());
    });
}

/// The well's single-keystroke archive action: `archiveContext` archives one
/// context — no latch, no subtree recursion — and is idempotent on replay.
#[test]
fn test_archive_context_rpc_is_single_context_and_idempotent() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        let ctx = kernel.create_context("solo-archive").await.unwrap();
        kernel.archive_context(ctx).await.unwrap();

        let contexts = kernel.list_contexts().await.unwrap();
        let row = contexts.iter().find(|c| c.id == ctx).unwrap();
        assert!(row.archived);

        // Idempotent: archiving again still succeeds.
        kernel.archive_context(ctx).await.unwrap();
    });
}

/// `setLastContext` auto-promotes a context that has never had an explicit
/// ring placement (design brief's "auto-promote on visit" rule) — but a
/// context that's been explicitly demoted stays demoted; explicit demotion
/// is sticky.
#[test]
fn test_set_last_context_auto_promotes_a_fresh_context_but_not_a_demoted_one() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;
        let client_id = uuid::Uuid::new_v4().to_string();

        let fresh = kernel.create_context("auto-promote-me").await.unwrap();
        kernel.set_last_context(&client_id, fresh).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = contexts.iter().find(|c| c.id == fresh).unwrap();
        assert!(
            row.promoted_at.is_some(),
            "a never-placed context should auto-promote on visit"
        );

        let demoted = kernel.create_context("stay-demoted").await.unwrap();
        kernel.demote_context(demoted).await.unwrap();
        kernel.set_last_context(&client_id, demoted).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = contexts.iter().find(|c| c.id == demoted).unwrap();
        assert!(
            row.promoted_at.is_none(),
            "explicit demotion is sticky — a visit must not re-promote it"
        );

        // Archived is sticky too: visits never resurrect — only an explicit
        // promote opens the resurrection door.
        let buried = kernel.create_context("stay-buried").await.unwrap();
        kernel.archive_context(buried).await.unwrap();
        kernel.set_last_context(&client_id, buried).await.unwrap();
        let contexts = kernel.list_contexts().await.unwrap();
        let row = contexts.iter().find(|c| c.id == buried).unwrap();
        assert!(row.archived, "a visit must not resurrect an archived context");
        assert!(row.promoted_at.is_none());
    });
}

/// The active ring's hard cap (10 seats) is enforced over the real wire:
/// promoting an 11th context fails loudly, and `setLastContext`'s
/// auto-promote silently skips (the visit itself must still succeed) rather
/// than erroring.
#[test]
fn test_active_ring_cap_enforced_over_rpc() {
    run_local(async {
        let addr = start_server().await;
        let (_client, kernel) = setup_execute_context(addr).await;

        for i in 0..10 {
            let ctx = kernel.create_context(&format!("cap-seat-{i}")).await.unwrap();
            kernel.promote_context(ctx).await.unwrap();
        }

        let overflow = kernel.create_context("cap-overflow").await.unwrap();
        let err = kernel.promote_context(overflow).await.unwrap_err();
        assert!(matches!(err, kaijutsu_client::RpcError::ServerError(_)));

        // Auto-promote-on-visit must not fail the visit itself when the ring
        // is full — it just silently skips the promotion.
        let client_id = uuid::Uuid::new_v4().to_string();
        kernel
            .set_last_context(&client_id, overflow)
            .await
            .expect("set_last_context must succeed even when auto-promote is skipped");
        let contexts = kernel.list_contexts().await.unwrap();
        let row = contexts.iter().find(|c| c.id == overflow).unwrap();
        assert!(
            row.promoted_at.is_none(),
            "auto-promote should have been skipped, not forced through"
        );
    });
}
