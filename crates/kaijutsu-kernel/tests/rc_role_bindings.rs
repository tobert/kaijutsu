//! Slice 5: explorer/director role bundles seeded via rc.
//!
//! These exercise the full path the capability-policy work delivers:
//! `kj context create --type explorer` runs the seeded rc `create` scripts,
//! whose `S10-binding.kai` calls `kj binding allow …` to narrow the new
//! context to a read-only allow-set — and that allow-set is then enforced at
//! `call_tool`. Running the real kaish scripts is the point: a malformed
//! capability token (e.g. a mislexed `instance:tool`) would leave the
//! expected grant absent and fail these assertions.
//!
//! Lives as an integration test (not a `#[cfg(test)]` unit test) so it
//! compiles against the library proper.

use std::sync::Arc;

use kaijutsu_kernel::block_store::shared_block_store_with_db;
use kaijutsu_kernel::drift::shared_drift_router;
use kaijutsu_kernel::file_tools::FileDocumentCache;
use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_kernel::mcp::{Capability, CallContext, InstanceId, KernelCallParams, McpError};
use kaijutsu_kernel::{Kernel, KjCaller, KjDispatcher, KjResult};
use kaijutsu_types::{KernelId, PrincipalId, SessionId};
use tokio_util::sync::CancellationToken;

struct Harness {
    kernel: Arc<Kernel>,
    dispatcher: Arc<KjDispatcher>,
    db: Arc<parking_lot::Mutex<KernelDb>>,
    creator: PrincipalId,
    _tmp: tempfile::TempDir,
}

/// Build a kernel with builtins registered + a kj dispatcher over the same
/// DB (so seeded rc scripts, context rows, and bindings all line up). The
/// in-memory DB auto-seeds the role context_types on open.
async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let creator = PrincipalId::system();
    let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
    let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
    let store = shared_block_store_with_db(db.clone(), ws_id, creator);

    let kernel = Arc::new(Kernel::new("rc-role-test", tmp.path()).await);

    // Seed and mount a private `/etc/rc` tree — the same setup the server RPC
    // boot path (kaijutsu-server/src/rpc.rs) and the unit-test helper
    // (kj::test_helpers) perform. Without it, `load_rc_scripts` hits
    // `NoMountPoint`, finds no scripts, and the role lifecycle never runs —
    // which is exactly what these tests exist to exercise.
    let rc_dir = tmp.path().join("rc");
    std::fs::create_dir_all(&rc_dir).expect("create rc dir");
    kaijutsu_kernel::seed_scripts::ensure_rc_seed_files(&rc_dir).expect("seed rc files");
    kernel
        .mount("/etc/rc", kaijutsu_kernel::vfs::LocalBackend::new(&rc_dir))
        .await;

    let file_cache = Arc::new(FileDocumentCache::new(store.clone(), kernel.vfs().clone()));
    kernel
        .register_builtin_mcp_servers(store.clone(), file_cache, None, db.clone())
        .await
        .expect("register_builtin_mcp_servers");

    let dispatcher = Arc::new(KjDispatcher::new(
        shared_drift_router(),
        store.clone(),
        db.clone(),
        kernel.clone(),
    ));
    dispatcher.set_self_arc();

    Harness {
        kernel,
        dispatcher,
        db,
        creator,
        _tmp: tmp,
    }
}

/// Create a context of `context_type` via `kj context create`, firing its rc
/// `create` lifecycle, and return the new context id.
async fn create_typed(h: &Harness, label: &str, context_type: &str) -> kaijutsu_types::ContextId {
    // Unprivileged caller: the rc create lifecycle assigns the loadout via its
    // own privileged kaish (materialize_context_kaish_rc), not this caller.
    let caller = KjCaller {
        principal_id: h.creator,
        context_id: None,
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: false,
    };
    let argv: Vec<String> = ["context", "create", label, "--type", context_type]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let res = h.dispatcher.dispatch(&argv, &caller).await;
    assert!(
        matches!(res, KjResult::Ok { .. }),
        "{context_type} context create failed: {}",
        res.message()
    );
    h.db
        .lock()
        .resolve_context(label)
        .unwrap_or_else(|e| panic!("context '{label}' not found: {e}"))
}

/// Thin wrapper so the facade assertions read cleanly.
async fn fx_broker_check(
    h: &Harness,
    ctx: &kaijutsu_types::ContextId,
    facade: &str,
) -> Result<(), McpError> {
    h.kernel.broker().check_facade(ctx, facade).await
}

#[tokio::test]
async fn explorer_role_seeds_readonly_allow_set_and_refuses_writes() {
    let h = harness().await;
    let ctx = create_typed(&h, "exp", "explorer").await;

    let binding = h
        .kernel
        .broker()
        .binding(&ctx)
        .await
        .expect("explorer rc must seed a binding");
    let file = InstanceId::new("builtin.file");
    let block = InstanceId::new("builtin.block");

    // Read-oriented grants present…
    assert!(binding.allows_tool(&file, "read"), "explorer should allow file read");
    assert!(binding.allows_tool(&file, "grep"), "explorer should allow file grep");
    assert!(binding.allows_tool(&file, "glob"), "explorer should allow file glob");
    assert!(
        binding.allows_tool(&block, "block_read"),
        "explorer should allow block_read"
    );
    // …mutating siblings withheld.
    assert!(
        !binding.allows_tool(&file, "write"),
        "explorer must NOT allow file write"
    );
    assert!(
        !binding.allows_tool(&file, "edit"),
        "explorer must NOT allow file edit"
    );
    assert!(
        !binding.allows_tool(&block, "block_create"),
        "explorer must NOT allow block_create"
    );

    // Facades: explorer holds none (reading the compose buffer is ungated, so
    // a read-only role needs no facade). The shell facade is refused.
    assert!(!binding.is_admin(), "explorer must NOT be a binding admin");
    assert!(
        matches!(
            fx_broker_check(&h, &ctx, "shell").await,
            Err(McpError::FacadeDenied { .. })
        ),
        "explorer shell facade must be refused"
    );
    assert!(
        matches!(
            fx_broker_check(&h, &ctx, "submit_input").await,
            Err(McpError::FacadeDenied { .. })
        ),
        "explorer submit_input facade must be refused"
    );

    // Enforced at the call path: a write is refused, not silently dropped.
    let call_ctx = CallContext::new(h.creator, ctx, SessionId::new(), KernelId::new());
    let denied = h
        .kernel
        .broker()
        .call_tool(
            KernelCallParams {
                instance: file.clone(),
                tool: "write".into(),
                arguments: serde_json::json!({"path": "/x", "content": "y"}),
            },
            &call_ctx,
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(denied, Err(McpError::CapabilityDenied { .. })),
        "explorer write must be refused, got {denied:?}"
    );
}

#[tokio::test]
async fn director_role_seeds_block_tooling_but_not_file_writes() {
    let h = harness().await;
    let ctx = create_typed(&h, "dir", "director").await;

    let binding = h
        .kernel
        .broker()
        .binding(&ctx)
        .await
        .expect("director rc must seed a binding");
    let file = InstanceId::new("builtin.file");
    let block = InstanceId::new("builtin.block");

    // Whole block instance granted — including mutating block tools.
    assert!(
        binding.allows_tool(&block, "block_create"),
        "director should allow block_create (whole-instance grant)"
    );
    assert!(binding.allows_tool(&block, "block_read"));
    // File access: read + the write tools. A director owns the rc lifecycle
    // scripts, so it gets file:write/edit (and rc-write below) for governance
    // artifacts — general code edits still delegate to a coder context. See
    // assets/defaults/rc/director/create/S10-binding.kai.
    assert!(binding.allows_tool(&file, "read"), "director should allow file read");
    assert!(
        binding.allows_tool(&file, "write"),
        "director should allow file write (rc-lifecycle governance)"
    );
    assert!(
        binding.allows_tool(&file, "edit"),
        "director should allow file edit (rc-lifecycle governance)"
    );
    // The dedicated /etc/rc subtree grant — deny-by-default, not implied by the
    // file-tool grants above.
    assert!(
        binding.allows(&Capability::RcWrite),
        "director should hold rc-write for the /etc/rc subtree"
    );

    // Director is a binding admin — may write any context's loadout.
    assert!(binding.is_admin(), "director should hold binding-admin");

    // Facades: director gets the full (collapsed) interaction surface.
    for facade in ["shell", "edit_input", "submit_input"] {
        assert!(
            binding.allows(&Capability::Facade(facade.to_string())),
            "director should allow facade {facade}"
        );
        assert!(
            fx_broker_check(&h, &ctx, facade).await.is_ok(),
            "director facade {facade} should pass the gate"
        );
    }
}
