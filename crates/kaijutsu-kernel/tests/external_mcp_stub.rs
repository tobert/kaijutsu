//! Real-handshake coverage for the external-MCP caller, using
//! `mcp_stub_server` (`src/bin/mcp_stub_server.rs`) — a minimal MCP stdio
//! server built as part of this same crate. Never kaibo or bevy_brp: those
//! need live binaries and, per the task that built this file, must not be
//! spawned from an automated test run that could collide with the shared
//! systemd `kaijutsu-server` other agents may be using at the same time.
//!
//! Lives in `tests/` (not inline `#[cfg(test)]`) because
//! `CARGO_BIN_EXE_mcp_stub_server` is only defined by Cargo for integration
//! tests within the binary's own package — not for `--lib` unit tests. See
//! `mcp/servers/external.rs`'s own inline tests for the "fails loudly"
//! connect() coverage that doesn't need a real MCP-speaking process.

use std::sync::Arc;
use std::time::Duration;

use kaijutsu_kernel::Kernel;
use kaijutsu_kernel::mcp::servers::{ExternalMcpServer, McpServerConfig};
use kaijutsu_kernel::mcp::{CallContext, Health, McpServerLike, external_instance_id};

fn stub_server_path() -> String {
    env!("CARGO_BIN_EXE_mcp_stub_server").to_string()
}

#[tokio::test]
async fn connect_succeeds_against_a_real_mcp_stdio_server() {
    let config = McpServerConfig {
        name: "stub".to_string(),
        command: stub_server_path(),
        ..Default::default()
    };
    let server = ExternalMcpServer::connect(
        config,
        kaijutsu_kernel::mcp::InstanceId::new("external.stub"),
        Duration::from_secs(5),
    )
    .await
    .expect("connect should succeed against the stub server");

    assert!(matches!(server.health().await, Health::Ready));
    let tools = server.list_tools(&CallContext::test()).await.unwrap();
    assert!(tools.is_empty(), "the stub server advertises no tools");

    server.shutdown().await.expect("shutdown should succeed");
}

/// The "add" path of `reconcile_with_toml` (`mcp::external_registry`)
/// against a real, successfully-connecting subprocess — proves it actually
/// lands a callable instance on the broker, not just that it attempted to.
/// The rest of the reconcile decision matrix (remove / leave-alone /
/// policy-refresh / malformed-entry / unstartable-entry) is covered by
/// `mcp::external_registry`'s own inline unit tests, which don't need a
/// real MCP-speaking process.
#[tokio::test]
async fn reconcile_adds_a_server_that_connects_successfully() {
    let kernel = Arc::new(Kernel::new_ephemeral("test").await);
    let stub = stub_server_path();
    let toml = format!(
        r#"
[servers.stub]
command = "{stub}"
"#
    );

    let report = kaijutsu_kernel::mcp::external_registry::reconcile_with_toml(&kernel, &toml).await;

    assert_eq!(report.added, vec!["stub".to_string()]);
    assert!(report.failed.is_empty(), "failed: {:?}", report.failed);
    let ids = kernel.broker().list_instances().await;
    assert!(ids.contains(&external_instance_id("stub")));
    assert!(
        kernel.broker().external_mcp_failures().await.unwrap().is_empty(),
        "a clean pass must clear any previous failure list"
    );
}
