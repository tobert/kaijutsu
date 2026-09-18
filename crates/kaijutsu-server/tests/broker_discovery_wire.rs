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
