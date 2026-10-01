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

/// A context-scoped declaration (`mcp::context_servers`) against a real
/// stdio process: it is granted to the declaring context alone, an unchanged
/// redeclaration keeps the running instance, and the server stops only when
/// its last owner lets go.
#[tokio::test]
async fn context_declared_server_lives_as_long_as_an_owner() {
    use kaijutsu_kernel::mcp::{
        Capability, ContextMcpServerSpec, ContextToolBinding, context_instance_id,
    };
    use kaijutsu_types::{ContextId, SessionId};

    let kernel = Arc::new(Kernel::new_ephemeral("test").await);
    let declaring = ContextId::new();
    let bystander = ContextId::new();
    let mut star = ContextToolBinding::new();
    star.grant(Capability::AllInstances);
    kernel.broker().set_binding(bystander, star).await.unwrap();

    let spec = ContextMcpServerSpec {
        name: "stub".to_string(),
        command: stub_server_path(),
        args: Vec::new(),
        env: Vec::new(),
    };
    let first = SessionId::new();
    let second = SessionId::new();
    let servers = kernel.context_mcp();
    let instance = context_instance_id(declaring, "stub");

    let ids = servers.declare(&kernel, declaring, first, vec![spec.clone()]).await.unwrap();
    assert_eq!(ids, vec![instance.clone()]);
    let running = kernel.broker().instances_snapshot().await[&instance].clone();
    let grant = Capability::Instance(instance.clone());
    assert!(kernel.broker().binding(&declaring).await.unwrap().allows(&grant));
    assert!(
        !kernel.broker().binding(&bystander).await.unwrap().allows(&grant),
        "`*` on another context must not reach a context-scoped server"
    );

    servers.declare(&kernel, declaring, second, vec![spec.clone()]).await.unwrap();
    let still = kernel.broker().instances_snapshot().await[&instance].clone();
    assert!(Arc::ptr_eq(&running, &still), "an unchanged declaration must not restart the server");

    servers.declare(&kernel, declaring, first, Vec::new()).await.unwrap();
    assert!(kernel.broker().list_instances().await.contains(&instance), "the second owner still holds it");

    servers.release_owner(&kernel, second).await;
    assert!(!kernel.broker().list_instances().await.contains(&instance));
    assert!(!kernel.broker().binding(&declaring).await.unwrap().allows(&grant));
    assert!(servers.list_all().await.is_empty());
}

fn stub_pid(pidfile: &std::path::Path) -> i32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(std::time::Instant::now() < deadline, "the stub never wrote its pidfile");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// True while `pid` has not exited; a zombie has.
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        let state = stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().next());
        state != Some("Z") && state != Some("X")
    })
}

/// A declared server must outlive the runtime of the connection that
/// declared it — each kaijutsu-server connection runs on its own runtime,
/// dropped when the connection closes — and must stop when the closing
/// connection's synchronous `Drop` hands its owner to
/// `release_owner_detached`, while the kernel keeps running.
#[test]
fn a_declared_server_outlives_the_declaring_runtime_until_released() {
    use kaijutsu_kernel::mcp::{ContextMcpServerSpec, ContextMcpServers};
    use kaijutsu_types::{ContextId, SessionId};

    let main = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let kernel = main.block_on(async { Arc::new(Kernel::new_ephemeral("test").await) });
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("stub.pid");
    let spec = ContextMcpServerSpec {
        name: "stub".to_string(),
        command: stub_server_path(),
        args: Vec::new(),
        env: vec![("MCP_STUB_PIDFILE".to_string(), pidfile.display().to_string())],
    };
    let context = ContextId::new();
    let owner = SessionId::new();

    // Declare from a short-lived current-thread runtime, as a connection does.
    let connection_rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let declaring = kernel.clone();
    connection_rt
        .block_on(async move { declaring.context_mcp().declare(&declaring, context, owner, vec![spec]).await })
        .unwrap();
    drop(connection_rt);

    let pid = stub_pid(&pidfile);
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "the server died with the runtime that declared it");

    ContextMcpServers::release_owner_detached(kernel.clone(), owner);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(std::time::Instant::now() < deadline, "the released server is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
    main.block_on(async {
        assert!(kernel.context_mcp().list_all().await.is_empty());
    });
}
