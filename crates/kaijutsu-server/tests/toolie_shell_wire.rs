//! The RPC shell follows the seat's shell facade: a seat holding only
//! `facade:shell` (a toolie) gets the read-only shell, and a seat holding
//! `facade:shell_write` gets the writable one.

mod common;

use common::{connect_client, create_context, create_context_typed, run_local, shell_exec_wait,
    start_server};
use kaijutsu_client::KernelHandle;
use kaijutsu_types::{BlockKind, BlockQuery, ContextId, Status};

/// Run `code` through the RPC shell and return the output block's status and
/// its stdout and stderr together.
async fn run(kernel: &KernelHandle, code: &str, context: ContextId) -> (Status, String) {
    let (command, _, status) = shell_exec_wait(kernel, code, context).await;
    let blocks = kernel.get_blocks(context, &BlockQuery::All).await.unwrap();
    let output = blocks.iter()
        .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(command))
        .expect("the command has an output block");
    (status, format!("{}\n{}", output.content, output.stderr.clone().unwrap_or_default()))
}

#[test]
fn a_toolie_seat_rpc_shell_refuses_writes_while_a_writable_seat_writes() {
    run_local(async {
        let addr = start_server().await;
        let scratch = tempfile::tempdir().unwrap();
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let toolie = create_context_typed(&kernel, "toolie-rpc-shell", "toolie").await.unwrap();
        let writer = create_context(&kernel, "writer-rpc-shell").await.unwrap();

        let write = format!("echo rpc-shell-probe > {}/probe", scratch.path().display());
        let (status, output) = run(&kernel, &write, writer).await;
        assert_eq!(status, Status::Done, "the writable seat must write: {output}");
        std::fs::remove_file(scratch.path().join("probe")).expect("the writable seat wrote the probe");

        let (status, output) = run(&kernel, &write, toolie).await;
        assert_eq!(status, Status::Error, "a toolie seat's RPC shell wrote: {output}");
        assert!(output.contains("permission denied"), "the write must be refused, not fail elsewhere: {output}");
        assert!(!scratch.path().join("probe").exists(), "a toolie seat's RPC shell wrote the probe");

        // The writable seat keeps its exports; the read-only one leaves the
        // context's env as it was, as the model's read-only `shell` does.
        for (context, kept) in [(writer, true), (toolie, false)] {
            let (status, output) = run(&kernel, "export RPC_SHELL_PROBE=kept", context).await;
            assert_eq!(status, Status::Done, "{output}");
            let (_, output) = run(&kernel, "echo \"probe=${RPC_SHELL_PROBE:-unset}\"", context).await;
            assert_eq!(output.contains("probe=kept"), kept, "kept={kept}: {output}");
        }

        let (status, output) = run(&kernel, "echo still-reads", toolie).await;
        assert_eq!(status, Status::Done, "a toolie seat's RPC shell still reads: {output}");
        assert!(output.contains("still-reads"), "{output}");
    });
}
