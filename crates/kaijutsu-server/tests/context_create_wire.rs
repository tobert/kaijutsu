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

#[test]
fn contexts_archive_and_restore_without_a_deletion_command() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let parent = choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap().context_id;
        let target = create_context(&kj, "retained-history").await.unwrap();
        let before = kj.get_blocks(target, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert!(!before.is_empty(), "created contexts must have instructions to retain");
        for verb in ["remove", "rm"] {
            let result = kj.execute_kj_quiet(parent, &["context".into(), verb.into(), target.to_hex(), "--confirm".into()]).await.unwrap();
            assert_ne!(result.exit_code, 0, "{verb} must not delete history");
            assert!(result.stderr.contains("unrecognized subcommand"), "{}", result.stderr);
        }
        let help = kj.execute_kj_quiet(parent, &["context".into(), "--help".into()]).await.unwrap();
        assert_eq!(help.exit_code, 0, "{}", help.stderr);
        println!("{}", help.stdout);
        assert!(!help.stdout.lines().any(|line| line.trim_start().starts_with("remove ")));
        for args in [vec!["context", "archive", "--help"], vec!["doc", "delete", "--help"]] {
            let help = kj.execute_kj_quiet(parent, &args.into_iter().map(String::from).collect::<Vec<_>>()).await.unwrap();
            assert_eq!(help.exit_code, 0, "{}", help.stderr);
            println!("{}", help.stdout);
        }
        let archive = kj.execute_kj_quiet(parent, &["context".into(), "archive".into(), target.to_hex(), "--confirm".into()]).await.unwrap();
        assert_eq!(archive.exit_code, 0, "{}", archive.stderr);
        assert!(kernel.kernel_db.lock().get_context(target).unwrap().unwrap().is_archived());
        let delete = kj.execute_kj_quiet(parent, &["doc".into(), "delete".into(), target.to_hex(), "--confirm".into()]).await.unwrap();
        assert_ne!(delete.exit_code, 0);
        assert!(delete.stderr.contains("archive"), "{}", delete.stderr);
        let retained = kj.get_blocks(target, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert_eq!(retained.iter().map(|b| (&b.id, &b.content)).collect::<Vec<_>>(), before.iter().map(|b| (&b.id, &b.content)).collect::<Vec<_>>());
        let restore = kj.execute_kj_quiet(parent, &["context".into(), "promote".into(), target.to_hex()]).await.unwrap();
        assert_eq!(restore.exit_code, 0, "{}", restore.stderr);
        assert!(!kernel.kernel_db.lock().get_context(target).unwrap().unwrap().is_archived());
        let file = kj.execute_kj_quiet(parent, &["doc".into(), "create".into(), "--kind".into(), "file".into()]).await.unwrap();
        assert_eq!(file.exit_code, 0, "{}", file.stderr);
        let file = kaijutsu_types::ContextId::parse(file.data.as_ref().unwrap()[0].as_str().unwrap()).unwrap();
        let deleted = kj.execute_kj_quiet(parent, &["doc".into(), "delete".into(), file.to_hex(), "--confirm".into()]).await.unwrap();
        assert_eq!(deleted.exit_code, 0, "{}", deleted.stderr);
        assert!(kernel.kernel_db.lock().get_document(file).unwrap().is_none());
        assert!(!kernel.documents.contains(file));
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
