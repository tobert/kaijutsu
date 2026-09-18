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
        assert_eq!(exit, 1, "cancelling result review reports the hook refusal, not the captured echo exit 0 or kaish cancellation 130");
    });
}

#[test]
fn interactive_exit_124_settles_consistently_in_receipt_blocks_and_job() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kj.join_context(context, "interactive-timeout").await.unwrap();
        let submission = kj.shell_submit("exit 124", context, true).await.unwrap();
        let operation = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let operation = kernel.kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
                if operation.completed_at.is_some() { break operation; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("the command must settle");
        let envelope = operation.envelope.unwrap();
        assert_eq!(envelope.exit_code, Some(124), "{envelope:?}");
        assert!(envelope.is_error());
        for block in [&operation.receipt.command_block_id, &operation.receipt.output_block_id] {
            assert_eq!(kernel.kernel.blocks().get_block_snapshot(context, block).unwrap().unwrap().status, Status::Error);
        }
        let jobs = kernel.kernel.context_job_manager(context);
        let job = jobs.list().await.into_iter().find(|job| Some(job.id.to_string()) == operation.receipt.job_id).unwrap();
        assert_eq!(jobs.wait(job.id).await.unwrap().code, 124);
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[test]
fn a_human_shell_command_is_performed_by_the_connected_human_not_the_context_performer() {
    run_local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = kaijutsu_server::SshServerConfig::ephemeral(addr.port());
        register_root_key(addr, config.root_key());
        let db_path = config.data_dir.as_ref().unwrap().join("kernel.db");
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            kaijutsu_server::SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
        });
        let kernel = kernel_rx.await.unwrap();
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "requester-performer").await.unwrap();
        let requester = kernel.kernel_db.lock().get_context(context).unwrap().unwrap().created_by;
        let (performer, reviewer) = (kaijutsu_types::PrincipalId::new(), kaijutsu_types::PrincipalId::new());
        {
            let db = kernel.kernel_db.lock();
            for (principal_id, name) in [(performer, "distinct-performer"), (reviewer, "distinct-reviewer")] {
                db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
                    principal_id, name: name.into(), created_at: 0, retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
            }
        }
        let set = kj.execute_kj_quiet(context, &[
            "context".into(), "set".into(), context.to_hex(), "--as".into(), "distinct-performer".into(),
            "--reviewer".into(), "distinct-reviewer".into(),
        ]).await.unwrap();
        assert_eq!(set.exit_code, 0, "{}", set.stderr);
        assert_eq!(kernel.kernel_db.lock().get_context(context).unwrap().unwrap().played_by, Some(performer));
        assert_ne!(requester, performer);
        kj.join_context(context, "requester-performer").await.unwrap();
        let submission = kj.shell_submit("echo who-ran-this", context, false).await.unwrap();
        let operation = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let operation = kernel.kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
                if operation.completed_at.is_some() { break operation; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("the command must settle");
        assert_eq!(operation.envelope.as_ref().unwrap().stdout, "who-ran-this\n");
        let (receipt_requester, receipt_performer): (Vec<u8>, Vec<u8>) = rusqlite::Connection::open(&db_path).unwrap().query_row(
            "SELECT principal_id, actor_id FROM shell_operations WHERE operation_id=?1",
            [&submission.operation_id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(receipt_requester, requester.as_bytes().to_vec(), "receipt requester");
        assert_eq!(receipt_performer, requester.as_bytes().to_vec(),
            "a direct human command is performed by the connected human (docs/approval-identity.md, \"Three identities\")");
        assert_ne!(receipt_performer, performer.as_bytes().to_vec(), "the context's performer does not run a human's command");
        let blocks = kernel.documents.block_snapshots(context).unwrap();
        let command = blocks.iter().find(|block| block.id == operation.receipt.command_block_id).unwrap();
        let output = blocks.iter().find(|block| block.id == operation.receipt.output_block_id).unwrap();
        assert_eq!(command.id.principal_id, requester, "the authored command block carries the connected human");
        assert_ne!(output.id.principal_id, performer, "the output block is not authored by the context's performer");
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
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

#[test]
fn archiving_preserves_an_accepted_commands_settlement_and_receipt() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let observer = kaijutsu_client::choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap().context_id;
        let context = create_context(&kj, "archive-during-settlement").await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("archive-during-settlement".into()), match_instance: None, match_tool: None,
            match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone() }) }),
        });
        let submission = kj.shell_submit("echo retained-after-archive", context, true).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.unwrap();
        let archive = kj.execute_kj_quiet(observer, &["context".into(), "archive".into(), context.to_hex(), "--confirm".into()]).await.unwrap();
        assert_eq!(archive.exit_code, 0, "{}", archive.stderr);
        release.notify_one();
        let operation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let operation = kernel.kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
                if operation.completed_at.is_some() { break operation; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("accepted commands must settle in retained history");
        let outcome = kernel.kernel.shell_operations().outcome(&submission.operation_id, context).unwrap().unwrap();
        let kaijutsu_kernel::runtime::command_outcome::CommandExecution::Completed(raw) = outcome.execution
            else { panic!("captured output was lost on archive") };
        assert_eq!(raw.text_out(), "retained-after-archive\n");
        assert!(operation.envelope.is_some());
        let blocks = kj.get_blocks(context, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert!(blocks.iter().any(|b| b.content.contains("retained-after-archive") && b.status.is_terminal()));
        assert!(kernel.kernel_db.lock().get_context(context).unwrap().unwrap().is_archived());
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[async_trait::async_trait]
impl Hook for PausedHook {
    async fn invoke(
        &self,
        _: &KernelCallParams,
        _: &CallContext,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> McpResult<()> {
        self.entered.notify_one();
        tokio::select! {
            _ = self.release.notified() => Ok(()),
            _ = cancel.cancelled() => Err(kaijutsu_kernel::mcp::McpError::Cancelled),
        }
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

/// OnError substitution on the interactive path: a command that fails before
/// it can run (an unterminated quote) has its error replaced by the hook, and
/// the durable output block and the operation receipt agree with the
/// replacement.
#[test]
fn interactive_on_error_replacement_agrees_with_durable_receipts() {
    run_local(async {
        use kaijutsu_kernel::mcp::KernelToolResult;
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        kj.join_context(context, "settlement-test").await.unwrap();
        {
            let mut hooks = kernel.kernel.broker().hooks().write().await;
            hooks.pre_call.entries.clear();
            hooks.post_call.entries.clear();
            hooks.on_error.entries.clear();
            hooks.on_error.entries.push(HookEntry {
                id: HookId("replace-error".into()), match_instance: None, match_tool: None,
                match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
                action: HookAction::ShortCircuit(KernelToolResult::text("recovered")),
            });
        }
        let submission = kj.shell_submit("echo '", context, true).await.unwrap();
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
        assert_eq!(output.status, Status::Done, "OnError substitution turns the failure into a success");
        assert_eq!(output.content, "recovered");
        assert_eq!(output.exit_code, None, "a synthetic replacement has no physical exit");
        let state = kernel.kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
        let envelope = state.envelope.expect("terminal block implies committed receipt");
        assert!(!envelope.is_error());
        assert_eq!(envelope.exit_code, None);
        assert_eq!(envelope.stdout, output.content);
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
