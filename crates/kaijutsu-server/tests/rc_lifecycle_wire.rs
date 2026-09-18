//! Inline rc re-entry shares retained command ownership.

mod common;

use common::{connect_client, create_context, run_local, start_server_with_kernel_handle};
use kaijutsu_kernel::vfs::VfsOps;
use kaijutsu_server::SharedKernel;
use kaijutsu_types::{ContextId, Status};
use std::path::Path;
use std::time::Duration;

async fn install_nested_lifecycle(server: &SharedKernel, child_label: &str) {
    let vfs = server.kernel.vfs();
    for path in ["/config/rc/nested-parent", "/config/rc/nested-parent/create",
        "/config/rc/nested-child", "/config/rc/nested-child/create"] {
        vfs.mkdir(Path::new(path), 0o755).await.unwrap();
    }
    vfs.write_all(Path::new("/config/rc/nested-parent/create/S00-child.kai"),
        format!("kj context create {child_label} --type nested-child").as_bytes()).await.unwrap();
    vfs.write_all(Path::new("/config/rc/nested-child/create/S00-wait.kai"),
        b"kj block create --role system --kind text --content child-entered\nwhile test ! -e /config/rc/release; do sleep 0.01; done\nkj block create --role system --kind text --content child-finished").await.unwrap();
    vfs.write_all(Path::new("/config/rc/nested-child/create/S10-later.kai"),
        b"kj block create --role system --kind text --content later-script").await.unwrap();
}

async fn wait_child(server: &SharedKernel, label: &str) -> ContextId {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let context = server.kernel_db.lock().list_active_contexts().unwrap().into_iter()
                .find(|row| row.label.as_deref() == Some(label)).map(|row| row.context_id);
            if let Some(context) = context {
                if server.documents.block_snapshots(context).unwrap().iter().any(|block| block.content == "child-entered") {
                    return context;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap_or_else(|_| {
        let contexts = server.kernel_db.lock().list_active_contexts().unwrap();
        let diagnostics: Vec<_> = contexts.iter().map(|row| {
            let blocks: Vec<_> = server.documents.block_snapshots(row.context_id).unwrap().into_iter()
                .filter(|block| matches!(block.kind, kaijutsu_types::BlockKind::Error | kaijutsu_types::BlockKind::ToolResult))
                .map(|block| (block.status, block.content, block.stderr)).collect();
            (&row.label, blocks)
        }).collect();
        panic!("nested child lifecycle must enter: {diagnostics:?}");
    })
}

#[test]
fn shutdown_joins_nested_rc_before_settling_the_command() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let parent = create_context(&kernel, "nested-shutdown").await.unwrap();
        install_nested_lifecycle(&server, "shutdown-child").await;
        let submission = kernel.shell_submit("kj context create nested-outer --type nested-parent", parent, false).await.unwrap();
        assert!(submission.refusal.is_none());
        let child = wait_child(&server, "shutdown-child").await;

        tokio::time::timeout(Duration::from_secs(3), server.kernel.shutdown_runtime_worker()).await
            .expect("shutdown must join nested rc cleanup").unwrap();
        let state = server.kernel.shell_operations().get(&submission.operation_id, parent).unwrap().unwrap();
        assert!(state.completed_at.is_some(), "accepted command must settle before shutdown returns");
        assert_ne!(state.envelope.as_ref().expect("terminal shell envelope").status,
            kaijutsu_types::shell_envelope::ShellStatus::Done, "cancelled execution must not claim success");
        let blocks = server.documents.block_snapshots(child).unwrap();
        assert!(blocks.iter().any(|block| block.content == "child-entered"));
        assert!(!blocks.iter().any(|block| block.content == "child-finished" || block.content == "later-script"));
        assert!(blocks.iter().any(|block| block.kind == kaijutsu_types::BlockKind::Error
            && block.content.contains("cancelled")), "nested cleanup must retain its diagnostic");
    });
}

#[test]
fn accepted_nested_rc_survives_disconnect_and_archive() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let parent = create_context(&kernel, "nested-disconnect").await.unwrap();
        kernel.join_context(parent, "nested-submitter").await.unwrap();
        let session = *server.session_contexts.iter().find(|row| *row.value() == parent).unwrap().key();
        install_nested_lifecycle(&server, "disconnect-child").await;
        let submission = kernel.shell_submit("kj context create nested-outer --type nested-parent", parent, false).await.unwrap();
        assert!(submission.refusal.is_none());
        let child = wait_child(&server, "disconnect-child").await;
        drop(kernel);
        drop(client);
        tokio::time::timeout(Duration::from_secs(3), async {
            while server.session_contexts.contains_key(&session) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("submitting SSH session must close");
        let outer = server.kernel_db.lock().find_context_by_label("nested-outer").unwrap().unwrap().context_id;
        for context in [parent, outer, child] {
            server.kernel_db.lock().archive_context(context).unwrap();
        }
        server.kernel.vfs().write_all(Path::new("/config/rc/release"), b"release").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = server.kernel.shell_operations().get(&submission.operation_id, parent).unwrap().unwrap();
                if state.completed_at.is_some() {
                    assert_eq!(state.envelope.as_ref().expect("terminal shell envelope").status,
                        kaijutsu_types::shell_envelope::ShellStatus::Done);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("accepted command must finish after disconnect and archive");
        let blocks = server.documents.block_snapshots(child).unwrap();
        assert!(blocks.iter().any(|block| block.content == "child-finished" && block.status == Status::Done));
        assert!(blocks.iter().any(|block| block.content == "later-script"));
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[test]
fn client_reads_complete_rc_output_from_successful_and_failed_scripts() {
    run_local(async {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let parent = create_context(&kernel, "rc-output-parent").await.unwrap();
        let vfs = server.kernel.vfs();
        for failed in [false, true] {
            let label = if failed { "rc-output-failure" } else { "rc-output-success" };
            let base = format!("/config/rc/{label}");
            vfs.mkdir(Path::new(&base), 0o755).await.unwrap();
            vfs.mkdir(Path::new(&format!("{base}/create")), 0o755).await.unwrap();
            let stdout = format!("stdout-first-{}-stdout-last", "音".repeat(3000));
            let stderr = format!("stderr-first-{}-stderr-last", "err".repeat(3000));
            let ending = if failed { "false" } else { "true" };
            let script = format!("printf '%s' '{stdout}'; printf '%s' '{stderr}' >&2; {ending}");
            vfs.write_all(Path::new(&format!("{base}/create/S00-output.kai")), script.as_bytes()).await.unwrap();
            let result = kernel.execute_kj(parent, &[
                "context".into(), "create".into(), label.into(), "--type".into(), label.into(),
            ]).await.unwrap();
            assert_eq!(result.exit_code, 0, "ordinary script failure retains the context: {}", result.stderr);
            let context = kernel.list_contexts().await.unwrap().into_iter()
                .find(|row| row.label == label).expect("created context").id;
            let blocks = kernel.get_blocks(context, &kaijutsu_types::BlockQuery::All).await.unwrap();
            let kind = if failed { kaijutsu_types::BlockKind::Error } else { kaijutsu_types::BlockKind::Trace };
            let capture = blocks.iter().find(|block| block.kind == kind).expect("rc output block over RPC");
            assert!(capture.content.contains(&stdout), "client must receive complete stdout");
            assert!(capture.content.contains(&stderr), "client must receive complete stderr");
            let listed = kernel.execute_kj(parent, &[
                "ledger".into(), "runs".into(), "--context".into(), label.into(),
            ]).await.unwrap();
            assert_eq!(listed.exit_code, 0, "{}", listed.stderr);
            let runs = listed.data.expect("structured run listing");
            let run_id = runs[0].as_str().expect("run id");
            let shown = kernel.execute_kj(parent, &[
                "ledger".into(), "runs".into(), run_id.into(),
            ]).await.unwrap();
            assert_eq!(shown.exit_code, 0, "{}", shown.stderr);
            let detail = shown.data.expect("structured run detail");
            assert_eq!(detail["outcome"], if failed { "failed" } else { "ok" });
            let script = &detail["scripts"][0];
            assert!(!script["projected_at"].is_null());
            assert_eq!(script["output_block_id"], capture.id.to_key());
            let returned: kaish_kernel::interpreter::ExecResult =
                serde_json::from_value(script["result"]["Completed"]["result"].clone()).unwrap();
            assert_eq!(returned.text_out(), stdout);
            assert_eq!(returned.err, stderr);
        }
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}
