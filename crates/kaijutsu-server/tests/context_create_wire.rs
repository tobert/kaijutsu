//! Creating a context over the wire goes through `kj context create`, run
//! from an existing context that becomes the parent (`docs/character.md`,
//! "Bootstrap: the person creates themself"). There is no `createContext`
//! RPC.

mod common;
use common::*;

use kaijutsu_client::{ParentSource, choose_parent, context_create_argv, context_id_from_create_result};
use kaijutsu_server::SshServerConfig;

#[test]
fn a_context_created_through_kj_is_parented_by_the_context_it_runs_from() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();

        let contexts = kj.list_contexts().await.unwrap();
        let parent = choose_parent(None, &contexts).expect("the ephemeral kernel has one root context");
        assert_eq!(parent.label, SshServerConfig::EPHEMERAL_ROOT);
        assert_eq!(parent.source, ParentSource::OnlyRoot);

        let parent_blocks = kernel.documents.block_snapshots(parent.context_id).unwrap().len();
        let result = kj
            .execute_kj_quiet(parent.context_id, &context_create_argv("lane", "coder", None))
            .await
            .unwrap();
        let lane = context_id_from_create_result(&result).expect("kj context create succeeds");

        let root = kernel.kernel_db.lock().get_character_by_name(SshServerConfig::EPHEMERAL_ROOT).unwrap().unwrap();
        let row = kernel.kernel_db.lock().get_context(lane).unwrap().unwrap();
        assert_eq!(row.forked_from, Some(parent.context_id));
        assert_eq!(row.created_by, root.principal_id);
        assert_eq!(row.director_id, Some(root.principal_id));
        assert_eq!(row.context_type, "coder");
        assert_eq!(row.played_by, None);
        assert_eq!(
            kernel.documents.block_snapshots(parent.context_id).unwrap().len(),
            parent_blocks,
            "a quiet create authors no blocks in the parent"
        );
    });
}

#[test]
fn a_duplicate_label_is_refused_with_a_label_conflict() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let parent = choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap();

        let first = kj.execute_kj_quiet(parent.context_id, &context_create_argv("dup", "coder", None)).await.unwrap();
        context_id_from_create_result(&first).expect("first create succeeds");
        let second = kj.execute_kj_quiet(parent.context_id, &context_create_argv("dup", "coder", None)).await.unwrap();
        let error = context_id_from_create_result(&second).expect_err("a live label cannot be reused");
        assert!(error.contains("label conflict"), "clients retry on this text: {error}");
    });
}
