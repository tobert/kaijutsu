//! Command completion is published only after result hooks settle.

mod common;
use common::*;
use std::sync::Arc;
use kaijutsu_kernel::mcp::{CallContext, Hook, HookAction, HookBody, HookEntry, HookId, KernelCallParams, McpResult};
use kaijutsu_types::Status;

struct PausedHook {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[test]
fn interactive_execution_survives_rpc_disconnect() {
    interactive_lifetime(false);
}

#[test]
fn interactive_submission_uses_its_addressed_context() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let joined = create_context(&kj, "interactive-joined").await.unwrap();
        let addressed = create_context(&kj, "interactive-addressed").await.unwrap();
        kj.join_context(joined, "interactive-address-test").await.unwrap();
        let submission = kj.shell_submit("kj context current", addressed, true).await.unwrap();
        let operation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let operation = kernel.kernel.shell_operations().get(&submission.operation_id, addressed).unwrap().unwrap();
                if operation.completed_at.is_some() { break operation; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        let envelope = operation.envelope.unwrap();
        assert!(!envelope.is_error(), "{envelope:?}");
        assert!(envelope.stdout.contains("interactive-addressed"), "{}", envelope.stdout);
        assert!(!envelope.stdout.contains("interactive-joined"));
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[test]
fn shutdown_settles_interactive_execution_paused_in_post_call() {
    interactive_lifetime(true);
}

#[test]
fn shutdown_settles_streaming_execution_paused_in_post_call() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kj.join_context(context, "streaming-lifetime").await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("streaming-lifetime".into()), match_instance: None, match_tool: None,
            match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: Arc::new(tokio::sync::Notify::new()) }) }),
        });
        let mut output = kj.subscribe_output().await.unwrap();
        let id = kj.execute("echo captured-stream").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), kernel.kernel.shutdown_runtime_worker())
            .await.expect("shutdown must join streaming settlement").unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(event) = output.recv().await {
                if let kaijutsu_client::OutputEvent::ExitCode { exec_id, code } = event {
                    if exec_id == id { return code; }
                }
            }
            panic!("streaming output closed without an exit");
        })
            .await.expect("joined shutdown must leave a completed streaming output");
        assert_ne!(exit, 0);
    });
}

#[test]
fn stopped_runtime_refuses_streaming_execution() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kj.join_context(context, "stopped-streaming").await.unwrap();
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
        let result = kj.execute("echo should-not-run").await;
        assert!(result.is_err(), "stopped runtime admitted streaming source");
    });
}

fn interactive_lifetime(shutdown: bool) {
    run_local(async move {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kj.join_context(context, "interactive-lifetime").await.unwrap();
        let session = *kernel.session_contexts.iter().find(|entry| *entry.value() == context).unwrap().key();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("interactive-lifetime".into()), match_instance: None, match_tool: None,
            match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone() }) }),
        });
        let submission = kj.shell_submit("echo retained-interactive", context, true).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.unwrap();
        if shutdown {
            tokio::time::timeout(std::time::Duration::from_secs(5), kernel.kernel.shutdown_runtime_worker())
                .await.expect("shutdown must join interactive settlement").unwrap();
        } else {
            drop(kj);
            drop(client);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while kernel.session_contexts.contains_key(&session) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }).await.expect("submitting RPC session must close");
            release.notify_one();
        }
        let operation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let operation = kernel.kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
                if operation.completed_at.is_some() { break operation; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("accepted interactive execution must settle without its submitting RPC task");
        let outcome = kernel.kernel.shell_operations().outcome(&submission.operation_id, context).unwrap().unwrap();
        let kaijutsu_kernel::runtime::command_outcome::CommandExecution::Completed(raw) = outcome.execution
            else { panic!("captured interactive output was lost") };
        assert_eq!(raw.text_out(), "retained-interactive\n");
        assert_eq!(operation.envelope.unwrap().is_error(), shutdown);
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[async_trait::async_trait]
impl Hook for PausedHook {
    async fn invoke(&self, _: &KernelCallParams, _: &CallContext) -> McpResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[test]
fn structured_kj_blocks_wait_for_post_call() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        let before: std::collections::HashSet<_> = kernel.documents.block_snapshots(context)
            .unwrap().into_iter().map(|block| block.id).collect();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("wait-for-post-call".into()), match_instance: None, match_tool: None,
            match_context: Some(context), match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone() }) }),
            priority: 0, kaish_script_id: None,
        });
        let argv = ["context".into(), "current".into()];
        let execute = kj.execute_kj(context, &argv);
        let observe = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.unwrap();
            let statuses: Vec<_> = kernel.documents.block_snapshots(context).unwrap().into_iter()
                .filter(|block| !before.contains(&block.id)).map(|block| block.status).collect();
            release.notify_one();
            statuses
        };
        let (result, statuses) = tokio::join!(execute, observe);
        assert_eq!(result.unwrap().exit_code, 0);
        assert_eq!(statuses, vec![Status::Running, Status::Running]);
        let final_statuses: Vec<_> = kernel.documents.block_snapshots(context).unwrap().into_iter()
            .filter(|block| !before.contains(&block.id)).map(|block| block.status).collect();
        assert_eq!(final_statuses, vec![Status::Done, Status::Done]);
    });
}

#[test]
fn interactive_hook_replacements_agree_with_durable_receipts() {
    run_local(async {
        use kaijutsu_kernel::mcp::KernelToolResult;
        use kaijutsu_kernel::runtime::command_outcome::CommandExecution;
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kj.join_context(context, "settlement-test").await.unwrap();
        for pre_call in [true, false] {
            let mut hooks = kernel.kernel.broker().hooks().write().await;
            hooks.pre_call.entries.clear();
            hooks.post_call.entries.clear();
            let table = if pre_call { &mut hooks.pre_call } else { &mut hooks.post_call };
            table.entries.push(HookEntry {
                id: HookId("replace-command".into()), match_instance: None, match_tool: None,
                match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
                action: HookAction::ShortCircuit(KernelToolResult::text("replacement")),
            });
            drop(hooks);
            let submission = kj.shell_submit("false", context, true).await.unwrap();
            let output = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let blocks = kj.get_blocks(context, &kaijutsu_types::BlockQuery::All).await.unwrap();
                    if let Some(output) = blocks.into_iter().find(|block| {
                        block.kind == kaijutsu_types::BlockKind::ToolResult
                            && block.tool_call_id == Some(submission.command_block_id)
                            && matches!(block.status, Status::Done | Status::Error)
                    }) { break output; }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            assert_eq!(output.status, Status::Done);
            assert_eq!(output.content, "replacement");
            assert_eq!(output.exit_code, None);
            let state = kernel.kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
            let envelope = state.envelope.expect("terminal block implies committed receipt");
            assert!(!envelope.is_error());
            assert_eq!(envelope.exit_code, None);
            assert_eq!(envelope.stdout, output.content);
            match kernel.kernel.shell_operations().outcome(&submission.operation_id, context).unwrap().unwrap().execution {
                CommandExecution::NotRun if pre_call => {}
                CommandExecution::Completed(result) if !pre_call => assert_eq!(result.code, 1),
                other => panic!("unexpected execution record: {other:?}"),
            }
        }
    });
}

#[test]
fn structured_kj_replacements_preserve_data_and_clear_execution_metadata() {
    run_local(async {
        use kaijutsu_kernel::mcp::{KernelToolResult, ToolContent};
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        let replacement = serde_json::json!({"reviewed": true});
        for pre_call in [false, true] {
            let mut hooks = kernel.kernel.broker().hooks().write().await;
            hooks.pre_call.entries.clear();
            hooks.post_call.entries.clear();
            let table = if pre_call { &mut hooks.pre_call } else { &mut hooks.post_call };
            table.entries.push(HookEntry {
                id: HookId("replace-kj".into()), match_instance: None, match_tool: None,
                match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
                action: HookAction::ShortCircuit(KernelToolResult { is_error: false,
                    content: vec![ToolContent::Text("reviewed".into())], structured: Some(replacement.clone()) }),
            });
            drop(hooks);
            for quiet in [false, true] {
                let argv = ["no-such-kj-verb".into()];
                let result = if quiet { kj.execute_kj_quiet(context, &argv).await }
                    else { kj.execute_kj(context, &argv).await }.unwrap();
                assert_eq!(result.exit_code, 0);
                assert_eq!(result.data, Some(replacement.clone()), "hook replacement data reaches the RPC caller");
                assert_eq!(result.stdout, "reviewed");
                assert!(result.stderr.is_empty());
                if let Some(command) = result.command_block_id {
                    let output = kernel.documents.block_snapshots(context).unwrap().into_iter()
                        .find(|block| block.tool_call_id == Some(command)).unwrap();
                    assert_eq!(output.status, Status::Done);
                    assert_eq!(output.exit_code, None, "a synthetic replacement has no physical exit");
                    assert!(output.stderr.is_none());
                    assert_eq!(output.output.unwrap().rich_json, Some(replacement.clone()));
                }
            }
        }
    });
}
