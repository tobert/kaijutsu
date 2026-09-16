//! Kernel start and root characters (`docs/character.md`, "Bootstrap: the
//! person creates themself"). A kernel with no live root character refuses
//! to start and names `kaijutsu-server init`. A kernel with one creates that
//! character's root context at start, once.

mod support;

use kaijutsu_types::PrincipalId;

async fn start(dir: &std::path::Path) -> Result<kaijutsu_server::SharedKernel, capnp::Error> {
    kaijutsu_server::rpc::create_shared_kernel(
        None,
        &kaijutsu_server::config_mounts::ConfigMounts::new(dir.join("config")),
        Some(dir),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kernel_without_a_root_character_refuses_to_start() {
    let tmp = tempfile::tempdir().unwrap();
    support::disable_embeddings(tmp.path());

    let Err(error) = start(tmp.path()).await else {
        panic!("a kernel with no root character must refuse to start");
    };
    let message = error.to_string();
    assert!(
        message.contains("kaijutsu-server init --as <name> --key <pubkey-file>"),
        "the refusal must say how to create the root character: {message}"
    );
    let contexts = kaijutsu_kernel::KernelDb::open(tmp.path().join("kernel.db"))
        .unwrap()
        .list_all_contexts()
        .unwrap();
    assert!(contexts.is_empty(), "a refused start creates no context, got {}", contexts.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_creates_the_root_context_once() {
    let tmp = tempfile::tempdir().unwrap();
    support::disable_embeddings(tmp.path());
    let amy = support::init_root(tmp.path(), "amy");

    let shared = start(tmp.path()).await.expect("a kernel with a root character starts");
    let contexts = shared.kernel_db.lock().list_active_contexts().unwrap();
    assert_eq!(contexts.len(), 1, "start creates exactly the root context");
    let root = &contexts[0];
    assert_eq!(root.label.as_deref(), Some("amy"));
    assert_eq!(root.context_type, "root");
    assert_eq!(root.played_by, Some(amy));
    assert_eq!(root.forked_from, None);
    assert_eq!(root.created_by, PrincipalId::system(), "the kernel creates it at start");
    assert!(
        shared.documents.contains(root.context_id),
        "the root context must have its conversation document"
    );
    assert_eq!(
        shared.kernel_db.lock().get_character(amy).unwrap().unwrap().root_ctx,
        Some(root.context_id)
    );
    let binding = shared.kernel.broker().binding(&root.context_id).await.expect("the root rc bundle binds it");
    assert!(binding.is_admin());
    drop(shared);

    let restarted = start(tmp.path()).await.expect("restart");
    let contexts = restarted.kernel_db.lock().list_active_contexts().unwrap();
    assert_eq!(contexts.len(), 1, "a restart creates no second root context");
}
