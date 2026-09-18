//! Context-visible broker schemas must survive the retained shell path.

mod common;

use common::{connect_client, create_context, run_local, shell_exec_wait, start_server_with_kernel_handle};
use kaijutsu_types::Status;

#[test]
fn qualified_file_tool_maps_positional_arguments_over_ssh() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "qualified-tool").await.unwrap();
        let binding = server.kernel.broker().binding(&context).await.unwrap();
        assert!(!binding.has_authority("exec"));
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "qualified tool schema reaches the client").unwrap();

        // Both builtin.file and builtin.resources publish `read`. The qualified
        // name must carry the file schema so this positional path becomes `path`.
        let command = format!("builtin_file__read '{}'", file.path().display());
        let (_, output, status) = shell_exec_wait(&kernel, &command, context).await;
        assert_eq!(status, Status::Done, "qualified native read failed: {output}");
        assert!(output.contains("qualified tool schema reaches the client"), "{output}");
    });
}

#[test]
fn image_tool_reads_mounted_bytes_through_the_retained_shell() {
    use kaijutsu_cas::{ContentHash, ContentStore};
    use kaijutsu_kernel::VfsOps;
    use kaijutsu_types::{ContentType, Role};
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let path = "/config/rc/pixel.svg";
        let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1"/></svg>"#;
        server.kernel.vfs().write_all(std::path::Path::new(path), bytes).await.unwrap();
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "mounted-image").await.unwrap();
        assert!(!server.kernel.broker().binding(&context).await.unwrap().has_authority("exec"));

        let (_, output, status) = shell_exec_wait(&kernel, &format!("img_block_from_path {path}"), context).await;
        assert_eq!(status, Status::Done, "mounted image import failed: {output}");
        let hash = ContentHash::from_data(bytes);
        let blocks = server.documents.block_snapshots(context).unwrap();
        let assets: Vec<_> = blocks.iter().filter(|block| block.role == Role::Asset).collect();
        assert_eq!(assets.len(), 1, "one import must create one asset");
        assert_eq!(assets[0].content_type, ContentType::Image);
        assert_eq!(assets[0].content, hash.to_string());
        assert_eq!(server.kernel.cas().retrieve(&hash).unwrap().unwrap(), bytes);
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
