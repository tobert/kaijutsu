//! File mutations retain the authenticated player through the ordinary shell.

mod common;

use common::{connect_client, create_context, run_local, shell_exec_wait, start_server_with_kernel_handle};
use kaijutsu_kernel::VfsOps;
use kaijutsu_types::{PrincipalId, Status};

#[test]
fn shell_redirection_attributes_cached_file_changes_to_the_player() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "file-input-actor").await.unwrap();
        let actor = server.kernel_db.lock().get_context(context).unwrap().unwrap().created_by;
        let prior_actor = PrincipalId::new();
        assert_ne!(prior_actor, actor);
        assert!(!server.kernel.broker().binding(&context).await.unwrap().has_authority("exec"));
        let path = "/config/rc/input-actor.txt";
        server.kernel.vfs().write_all(std::path::Path::new(path), b"original\n").await.unwrap();
        let target = kaijutsu_kernel::editor::resolve_editor_target(path, server.kernel.file_cache()).await.unwrap();
        server.documents.edit_text_as(target.context_id, &target.block_id, 0, "prior:", 0, Some(prior_actor)).unwrap();
        server.kernel.file_cache().mark_dirty(path).unwrap();
        server.kernel.file_cache().flush_one(path).await.unwrap();
        assert_eq!(server.documents.get(target.context_id).unwrap().doc.principal_id(), prior_actor);

        let (_, output, status) = shell_exec_wait(&kernel, &format!("echo replacement > {path}"), context).await;
        assert_eq!(status, Status::Done, "file replacement failed: {output}");
        let block = server.documents.get_block_snapshot(target.context_id, &target.block_id).unwrap().unwrap();
        assert_eq!(block.content, "replacement\n");
        assert_eq!(block.id, target.block_id, "replacement retains the original block author");
        assert_eq!(server.documents.get(target.context_id).unwrap().doc.principal_id(), actor,
            "cached file mutation must use the current player, not the prior editor or store default");
        assert_eq!(server.kernel.vfs().read_all(std::path::Path::new(path)).await.unwrap(), b"replacement\n");
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
