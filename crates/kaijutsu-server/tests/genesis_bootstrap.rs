//! Genesis bootstrap: a brand-new kernel (empty data dir → zero contexts)
//! seeds exactly one fully-privileged `coder` context at cold start, so the
//! app is usable without a manual `kj context create`.
//!
//! Trigger is strictly *zero contexts at cold start* — once any context exists
//! the `all_contexts.is_empty()` guard suppresses reseeding (recovery loads the
//! existing ones first). We test the positive path here; the negative is the
//! guard itself, exercised by every non-empty cold start in the other e2e tests.

use kaijutsu_types::PrincipalId;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_kernel_seeds_one_genesis_coder_context() {
    let tmp = tempfile::tempdir().unwrap();

    // config_dir = None → embedded rc/config defaults; data_dir = fresh tempdir
    // → an empty KernelDb, so the genesis bootstrap must fire.
    let shared = kaijutsu_server::rpc::create_shared_kernel(None, Some(tmp.path()))
        .await
        .expect("create_shared_kernel should succeed on an empty data dir");

    let contexts = {
        let db = shared.kernel_db.lock();
        db.list_active_contexts().expect("list_active_contexts")
    };

    assert_eq!(
        contexts.len(),
        1,
        "an empty kernel should seed exactly one genesis context, got {}",
        contexts.len()
    );

    let genesis = &contexts[0];
    assert_eq!(
        genesis.context_type, "coder",
        "genesis must be a coder context (the fully-privileged loadout)"
    );
    assert_eq!(
        genesis.label.as_deref(),
        Some("genesis"),
        "genesis context should carry the 'genesis' label"
    );
    assert_eq!(
        genesis.created_by,
        PrincipalId::system(),
        "genesis is authored by the system principal"
    );

    // The full create recipe ran: the Conversation document exists for it.
    assert!(
        shared.documents.contains(genesis.context_id),
        "genesis context must have its Conversation document created"
    );
}
