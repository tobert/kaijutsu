//! Archive refuses new execution while retaining explicit history reads.

mod common;
use common::*;

#[test]
fn archived_context_refuses_interactive_shell_admission() { archived_submission("interactive"); }

#[test]
fn archived_context_refuses_structured_command_admission() { archived_submission("structured"); }

#[test]
fn archived_context_refuses_quiet_command_admission() { archived_submission("quiet"); }

#[test]
fn archived_context_refuses_streaming_shell_admission() { archived_submission("streaming"); }

#[test]
fn archived_context_refuses_prompt_without_authoring_input() { archived_submission("prompt"); }

#[test]
fn archived_context_refuses_chat_submit_without_consuming_draft() { archived_submission("chat-draft"); }

#[test]
fn archived_context_refuses_shell_submit_without_consuming_draft() { archived_submission("shell-draft"); }

fn archived_submission(path: &'static str) {
    run_local(async move {
        let (addr, server) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let observer = kaijutsu_client::choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap().context_id;
        let target = create_context(&kj, "archived-admission").await.unwrap();
        kj.join_context(target, "archive-admission").await.unwrap();
        if path.ends_with("-draft") {
            kj.edit_input(target, 0, "retained draft", 0).await.unwrap();
        }
        let archive = kj.execute_kj_quiet(observer, &["context".into(), "archive".into(), target.to_hex(), "--confirm".into()]).await.unwrap();
        assert_eq!(archive.exit_code, 0, "{}", archive.stderr);
        let before = kj.get_blocks(target, &kaijutsu_types::BlockQuery::All).await.unwrap();
        let result = match path {
            "interactive" => kj.shell_submit("echo must-not-run", target, true).await.map(|_| ()),
            "structured" => kj.execute_kj(target, &["context".into(), "current".into()]).await.map(|_| ()),
            "quiet" => kj.execute_kj_quiet(target, &["context".into(), "current".into()]).await.map(|_| ()),
            "streaming" => kj.execute("echo must-not-run").await.map(|_| ()),
            "prompt" => kj.prompt("must not become a user block", None, target).await.map(|_| ()),
            "chat-draft" => kj.submit_input(target, false).await.map(|_| ()),
            "shell-draft" => kj.submit_input(target, true).await.map(|_| ()),
            _ => unreachable!(),
        };
        server.kernel.shutdown_runtime_worker().await.unwrap();
        let error = result.expect_err("archived context must refuse new execution before returning a handle");
        assert!(error.to_string().contains("archived"), "{error}");
        let after = kj.get_blocks(target, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert_eq!(after.iter().map(|b| (b.id, &b.content, b.status, b.ephemeral)).collect::<Vec<_>>(),
            before.iter().map(|b| (b.id, &b.content, b.status, b.ephemeral)).collect::<Vec<_>>());
        if path.ends_with("-draft") {
            assert_eq!(kj.get_input_state(target).await.unwrap().content, "retained draft");
        }
        assert!(server.kernel.shell_operations().list_for_context(target).unwrap().is_empty(),
            "refused admission must not leave an execution receipt");
    });
}
