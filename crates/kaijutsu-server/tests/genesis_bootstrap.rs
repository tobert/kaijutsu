//! ROOT bootstrap: a brand-new kernel (empty data dir → zero contexts) seeds
//! exactly one `director` context, `ROOT`, at cold start — the binding-admin
//! root of the tree. ROOT deliberately can't drive turns itself (no drive/fork
//! authority); the operator creates a coder (or any other type) from it when a
//! conversational context is needed.
//!
//! Trigger is strictly *zero contexts at cold start* — once any context exists
//! the `all_contexts.is_empty()` guard suppresses reseeding (recovery loads the
//! existing ones first). We test the positive path here; the negative is the
//! guard itself, exercised by every non-empty cold start in the other e2e tests.

use kaijutsu_types::PrincipalId;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_kernel_seeds_one_root_director_context() {
    let tmp = tempfile::tempdir().unwrap();

    // config_dir = None → embedded rc/config defaults; data_dir = fresh tempdir
    // → an empty KernelDb, so the ROOT bootstrap must fire.
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
        "an empty kernel should seed exactly one ROOT context, got {}",
        contexts.len()
    );

    let root = &contexts[0];
    assert_eq!(
        root.context_type, "director",
        "ROOT must be a director context (the binding-admin loadout)"
    );
    assert_eq!(
        root.label.as_deref(),
        Some("ROOT"),
        "the bootstrap context should carry the 'ROOT' label"
    );
    assert_eq!(
        root.created_by,
        PrincipalId::system(),
        "ROOT is authored by the system principal"
    );

    // The full create recipe ran: the Conversation document exists for it.
    assert!(
        shared.documents.contains(root.context_id),
        "ROOT context must have its Conversation document created"
    );
}
