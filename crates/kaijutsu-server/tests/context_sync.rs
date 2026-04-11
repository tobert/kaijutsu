//! Integration tests for RPC and Shell context synchronization.

mod common;
use common::*;
use kaijutsu_client::OutputEvent;

#[test]
fn test_rpc_join_updates_shell() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // 1. Create and join a context via RPC
        let ctx_id = kernel.create_context("sync-test").await.unwrap();
        kernel.join_context(ctx_id, "test-instance").await.unwrap();

        // 2. Warm up EmbeddedKaish (ensures KjBuiltin is registered)
        let mut rx = kernel.subscribe_output().await.unwrap();
        let warm_id = kernel.execute("echo warm").await.unwrap();
        wait_for_exit_code(&mut rx, warm_id).await;

        // 3. Execute a shell command that reports the current context
        let exec_id = kernel.execute("kj context current").await.unwrap();
        
        let events = collect_output_events(&mut rx, exec_id).await;
        let stdout = events.iter().filter_map(|e| {
            if let OutputEvent::Stdout { text, .. } = e { Some(text.clone()) } else { None }
        }).collect::<String>();

        // 4. Verify shell is in the correct context
        assert!(stdout.contains("sync-test"), "Shell stdout should contain context label. Got: {}", stdout);
        assert!(stdout.contains(&ctx_id.short()), "Shell stdout should contain context short ID");
    });
}

#[test]
fn test_shell_switch_updates_rpc() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // 1. Create two contexts
        let ctx_a = kernel.create_context("alpha").await.unwrap();
        let ctx_b = kernel.create_context("beta").await.unwrap();

        // 2. Join alpha initially
        kernel.join_context(ctx_a, "test-instance").await.unwrap();

        // 3. Switch to beta via shell
        let mut rx = kernel.subscribe_output().await.unwrap();
        let exec_id = kernel.execute("kj context switch beta").await.unwrap();
        let exit_code = wait_for_exit_code(&mut rx, exec_id).await;
        assert_eq!(exit_code, 0, "kj context switch failed");

        // 4. Verify RPC layer now thinks we are in beta by performing a context-dependent operation
        // We'll use a shell command again, but this time it validates that 'EmbeddedKaish' 
        // picked up the change from the unified map.
        let exec_id = kernel.execute("kj context current").await.unwrap();
        let events = collect_output_events(&mut rx, exec_id).await;
        let stdout = events.iter().filter_map(|e| {
            if let OutputEvent::Stdout { text, .. } = e { Some(text.clone()) } else { None }
        }).collect::<String>();

        assert!(stdout.contains("beta"), "After shell switch, kj context current should show beta. Got: {}", stdout);
        
        // 5. Also verify via a direct RPC that requires context (like get_blocks)
        // If require_context() was stale, it would still point to alpha or fail.
        let blocks = kernel.get_blocks(ctx_b, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert!(blocks.is_empty(), "Should be able to query blocks for beta");
    });
}

#[test]
fn test_session_isolation_unified() {
    run_local(async {
        let addr = start_server().await;
        
        // 1. Connect Client A and join 'alpha'
        let client_a = connect_client(addr).await;
        let (kernel_a, _) = client_a.attach_kernel().await.unwrap();
        let ctx_a = kernel_a.create_context("alpha").await.unwrap();
        kernel_a.join_context(ctx_a, "instance-a").await.unwrap();

        // 2. Connect Client B and join 'beta'
        let client_b = connect_client(addr).await;
        let (kernel_b, _) = client_b.attach_kernel().await.unwrap();
        let ctx_b = kernel_b.create_context("beta").await.unwrap();
        kernel_b.join_context(ctx_b, "instance-b").await.unwrap();

        // 3. Verify Client A is still in alpha
        let mut rx_a = kernel_a.subscribe_output().await.unwrap();
        let exec_a = kernel_a.execute("kj context current").await.unwrap();
        let events_a = collect_output_events(&mut rx_a, exec_a).await;
        let stdout_a = events_a.iter().filter_map(|e| {
            if let OutputEvent::Stdout { text, .. } = e { Some(text.clone()) } else { None }
        }).collect::<String>();
        assert!(stdout_a.contains("alpha"), "Session A should be in alpha. Got: {}", stdout_a);

        // 4. Verify Client B is still in beta
        let mut rx_b = kernel_b.subscribe_output().await.unwrap();
        let exec_b = kernel_b.execute("kj context current").await.unwrap();
        let events_b = collect_output_events(&mut rx_b, exec_b).await;
        let stdout_b = events_b.iter().filter_map(|e| {
            if let OutputEvent::Stdout { text, .. } = e { Some(text.clone()) } else { None }
        }).collect::<String>();
        assert!(stdout_b.contains("beta"), "Session B should be in beta. Got: {}", stdout_b);

        // 5. Client A switches to beta via shell
        let exec_sw = kernel_a.execute("kj context switch beta").await.unwrap();
        wait_for_exit_code(&mut rx_a, exec_sw).await;

        // 6. Verify Session A moved, but Session B stayed in beta (wait, B is already in beta)
        // Let's have A switch to a third context 'gamma'
        let _ctx_g = kernel_a.create_context("gamma").await.unwrap();
        let exec_g = kernel_a.execute("kj context switch gamma").await.unwrap();
        wait_for_exit_code(&mut rx_a, exec_g).await;

        let exec_a2 = kernel_a.execute("kj context current").await.unwrap();
        let events_a2 = collect_output_events(&mut rx_a, exec_a2).await;
        let stdout_a2 = events_a2.iter().filter_map(|e| {
            if let OutputEvent::Stdout { text, .. } = e { Some(text.clone()) } else { None }
        }).collect::<String>();
        assert!(stdout_a2.contains("gamma"), "Session A should have moved to gamma");

        let exec_b2 = kernel_b.execute("kj context current").await.unwrap();
        let events_b2 = collect_output_events(&mut rx_b, exec_b2).await;
        let stdout_b2 = events_b2.iter().filter_map(|e| {
            if let OutputEvent::Stdout { text, .. } = e { Some(text.clone()) } else { None }
        }).collect::<String>();
        assert!(stdout_b2.contains("beta"), "Session B should still be in beta");
    });
}

// ============================================================================
// Helpers (duplicated from rpc_integration.rs for now)
// ============================================================================

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

async fn collect_output_events(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<kaijutsu_client::OutputEvent>,
    exec_id: u64,
) -> Vec<kaijutsu_client::OutputEvent> {
    let mut events = Vec::new();
    loop {
        match rx.recv().await {
            Some(event) => {
                let id = match &event {
                    kaijutsu_client::OutputEvent::Stdout { exec_id: id, .. } => *id,
                    kaijutsu_client::OutputEvent::Stderr { exec_id: id, .. } => *id,
                    kaijutsu_client::OutputEvent::ExitCode { exec_id: id, .. } => *id,
                };
                if id == exec_id {
                    let is_exit = matches!(event, kaijutsu_client::OutputEvent::ExitCode { .. });
                    events.push(event);
                    if is_exit { return events; }
                }
            }
            None => panic!("output channel closed before exit code received"),
        }
    }
}
