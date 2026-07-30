//! Boot-level coverage for external MCP server reconciliation
//! (`rpc::start_external_mcp_servers`, called from
//! `SshServer::run_on_listener` immediately after the kernel is built): a
//! misconfigured `mcp.toml` must never abort startup, and a
//! configured-but-unstartable server must be visibly failed rather than
//! silently absent.
//!
//! The reconcile is invoked EXPLICITLY here, exactly as the serving path
//! does, instead of riding along inside `create_shared_kernel`. That
//! separation is the point: constructing a kernel must not spawn host
//! subprocesses, or every kernel-booting test in the workspace launches a
//! real kaibo against `~/.local/state/kaibo/state.db`.
//!
//! Mirrors `genesis_bootstrap.rs`'s
//! `config_dir` tempdir-override pattern (`config_seed_override` reads a
//! host file as a one-time seed source only on a fresh, empty config
//! namespace).
//!
//! No real MCP-speaking subprocess here — see
//! `kaijutsu-kernel/tests/external_mcp_stub.rs` for that (a same-package-only
//! `CARGO_BIN_EXE_*` fixture doesn't cross into this crate's tests). This
//! file exercises the "doesn't abort boot" / "visibly failed" guarantees
//! with a deliberately-broken mcp.toml instead.

use kaijutsu_kernel::mcp::InstanceId;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_mcp_toml_does_not_abort_boot_and_is_visibly_failed() {
    let config_dir = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();

    // One well-formed-but-unreachable entry (component 2's core scenario —
    // e.g. a typo'd kaibo path) and one entry malformed enough that the
    // *loader* drops it (component 1). Neither may take the kernel down.
    std::fs::write(
        config_dir.path().join("mcp.toml"),
        r#"
[servers.ghost]
command = "/definitely/does/not/exist/kaibo"

[servers.broken]
transport = "carrier_pigeon"
"#,
    )
    .unwrap();

    let shared = kaijutsu_server::rpc::create_shared_kernel(
        Some(config_dir.path()),
        Some(data_dir.path()),
    )
    .await
    .expect("a bad mcp.toml entry must not fail kernel boot");

    // Building the kernel must not have touched external MCP at all — the
    // reconcile is the serving path's job. If this ever starts returning
    // Some, construction has quietly regained the subprocess-spawning side
    // effect this split exists to prevent.
    assert!(
        shared.kernel.broker().external_mcp_failures().await.is_none(),
        "create_shared_kernel must not reconcile external MCP servers"
    );

    // Now do what `run_on_listener` does.
    kaijutsu_server::rpc::start_external_mcp_servers(&shared.kernel).await;

    // Visibly failed, not silently absent: neither entry made it onto the
    // broker...
    let ids = shared.kernel.broker().list_instances().await;
    assert!(
        !ids.contains(&InstanceId::new("external.ghost")),
        "an unstartable server must not appear registered"
    );
    assert!(!ids.contains(&InstanceId::new("external.broken")));

    // ...but the failure is queryable, not just a log line that scrolled by.
    let failures = shared
        .kernel
        .broker()
        .external_mcp_failures()
        .await
        .expect("reconcile should have run during boot and recorded a result");
    let names: Vec<&str> = failures.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"ghost"), "failures: {failures:?}");
    assert!(
        failures.iter().any(|(_, reason)| !reason.is_empty()),
        "the recorded failure must carry a reason, not just a name"
    );
}
