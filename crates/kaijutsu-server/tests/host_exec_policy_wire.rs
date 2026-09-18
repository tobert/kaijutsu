//! Ephemeral server fixtures deny host subprocesses unless a test opts in.

mod common;

use common::{
    connect_client, create_context, run_local, shell_exec_wait, start_server,
    start_server_with_host_exec,
};
use kaijutsu_types::Status;

#[test]
fn ephemeral_server_denies_host_exec() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "no-host-exec").await.unwrap();

        let (_, output, status) = shell_exec_wait(&kernel, "/usr/bin/printf denied", context).await;

        assert_eq!(status, Status::Error, "external command unexpectedly ran: {output}");
    });
}

#[test]
fn ephemeral_server_allows_host_exec_only_after_opt_in() {
    run_local(async {
        let addr = start_server_with_host_exec().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "host-exec").await.unwrap();

        let (_, output, status) = shell_exec_wait(&kernel, "/usr/bin/printf allowed", context).await;

        assert_eq!(status, Status::Done, "opted-in external command failed: {output}");
        assert_eq!(output, "allowed");
    });
}
