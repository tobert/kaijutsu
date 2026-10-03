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

/// On a context whose create lifecycle installs the shell-escape guard,
/// `python3 -c` through the person's shell passes the guard and runs as the
/// wrapped command, while `sh -c` is still denied. Skips when the host has
/// no `python3`.
#[test]
fn python_inline_code_passes_the_guard_and_runs_wrapped() {
    let has_python = std::env::var("PATH").ok()
        .and_then(|path| kaish_kernel::tools::wrapped::find_executable("python3", &path)).is_some();
    if !has_python {
        eprintln!("SKIP: no python3 on the test process PATH");
        return;
    }
    run_local(async {
        let addr = start_server_with_host_exec().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "python-guard").await.unwrap();

        let denied = kernel.shell_execute("sh -c 'echo hi'", context, false).await
            .expect_err("the guard must be active on this context");
        assert!(denied.to_string().contains("shell-escape-guard"), "{denied}");

        let (_, output, status) = shell_exec_wait(&kernel, "python3 -c 'print(6*7)'", context).await;
        assert_eq!(status, Status::Done, "python3 -c must pass the guard: {output}");
        assert_eq!(output.trim(), "42");

        let (_, output, status) = shell_exec_wait(&kernel, "type -t python3", context).await;
        assert_eq!(status, Status::Done, "{output}");
        assert_eq!(output.trim(), "builtin", "python3 must be the wrapped command");
    });
}
