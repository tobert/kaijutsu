//! Startup interruption reaches both block readers and durable operation polls.

mod common;

use std::time::Duration;
use kaijutsu_client::{CallError, KeySource, SshConfig, spawn_actor};
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::{OutputData, Status};

#[test]
fn interrupted_shell_keeps_observations_through_the_typed_client() {
    common::run_local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        let key = config.root_key();
        let path = config.data_dir.as_ref().unwrap().join("kernel.db");
        let marker = config.data_dir.as_ref().unwrap().join("must-not-run");
        let (context, command, output, orphan_call, orphan_result) = {
            let shared = kaijutsu_server::rpc::create_shared_kernel(
                config.config_dir.as_deref(), &config.config_mounts, config.data_dir.as_deref(), &[],
            ).await.unwrap();
            let amy = shared.kernel_db.lock().get_character_by_name("amy").unwrap().unwrap().principal_id;
            let context = shared.kernel_db.lock().get_character(amy).unwrap().unwrap().root_ctx.unwrap();
            let blocks = &shared.documents;
            let command = blocks.insert_tool_call(context, None, None, "shell", serde_json::json!({}), None).unwrap();
            let output = blocks.insert_tool_result(context, &command, Some(&command), "observed stdout", false, None, None).unwrap();
            for id in [&command, &output] { blocks.set_status(context, id, Status::Running).unwrap(); }
            blocks.set_stderr(context, &output, Some("observed stderr".into())).unwrap();
            blocks.set_output(context, &output, Some(&OutputData::new().with_rich_json(serde_json::json!({"observed": 1})))).unwrap();
            let orphan_call = blocks.insert_tool_call(context, Some(&output), Some(&output), "model_tool", serde_json::json!({}), None).unwrap();
            let orphan_result = blocks.insert_tool_result(context, &orphan_call, Some(&orphan_call), "partial tool output", false, None, None).unwrap();
            blocks.set_status(context, &orphan_result, Status::Waiting).unwrap();
            blocks.set_stderr(context, &orphan_result, Some("partial tool stderr".into())).unwrap();
            // Persist the state of a writer that departed before recording its outcome.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("INSERT INTO shell_operations(operation_id,context_id,principal_id,actor_id,command_block_id,output_block_id,source,created_at)
                VALUES('interrupted',?1,?2,?2,?3,?4,?5,1)", rusqlite::params![
                context.as_bytes().to_vec(), amy.as_bytes().to_vec(), command.to_key(), output.to_key(),
                format!("echo unexpected > '{}'", marker.display()),
            ]).unwrap();
            conn.execute("INSERT INTO execution_notifications(kind,source_id) VALUES('shell','interrupted')", []).unwrap();
            shared.kernel.shutdown_runtime_worker().await.unwrap();
            (context, command, output, orphan_call, orphan_result)
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
        });
        let kernel = rx.await.unwrap();
        let actor = spawn_actor(SshConfig {
            host: addr.ip().to_string(), port: addr.port(), username: "amy".into(),
            key_source: KeySource::InMemory(key), insecure: true,
        }, None, "interruption-wire".into(), false);
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match actor.whoami().await {
                    Ok(_) => break,
                    Err(CallError::NotReady(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
                    Err(error) => panic!("client connection failed: {error}"),
                }
            }
        }).await.expect("client connects");
        for argv in [vec!["wait", "--help"], vec!["ledger", "show", "--help"]] {
            let help = actor.execute_kj_quiet(context, argv.into_iter().map(str::to_owned).collect()).await.unwrap();
            assert_eq!(help.exit_code, 0);
            println!("{}", help.stdout);
        }
        for id in [command, output, orphan_call, orphan_result] {
            assert_eq!(actor.get_block(context, id).await.unwrap().unwrap().status, Status::Error);
        }
        let result = actor.get_block(context, output).await.unwrap().unwrap();
        assert_eq!(result.content, "observed stdout");
        assert!(result.is_error);
        assert_eq!(result.exit_code, None);
        assert!(result.stderr.as_deref().unwrap().starts_with("observed stderr\n"));
        assert!(result.stderr.as_deref().unwrap().contains("restarted"));
        assert_eq!(result.output.unwrap().to_json(), serde_json::json!({"observed": 1}));
        let orphan = actor.get_block(context, orphan_result).await.unwrap().unwrap();
        assert_eq!(orphan.content, "partial tool output");
        assert!(orphan.is_error);
        assert_eq!(orphan.exit_code, None);
        assert!(orphan.stderr.as_deref().unwrap().starts_with("partial tool stderr\n"));
        assert!(orphan.stderr.as_deref().unwrap().contains("restarted"));
        for _ in 0..2 {
            let result = actor.execute_kj_quiet(context, vec!["wait".into(), "--operation".into(), "interrupted".into(), "--timeout".into(), "0".into()]).await.unwrap();
            assert_eq!(result.exit_code, 0, "operation polling itself succeeds: {}", result.stderr);
            let data = result.data.unwrap();
            assert_eq!(data["timed_out"], false);
            let envelope = &data["state"]["envelope"];
            assert_eq!(envelope["status"], "error");
            assert_eq!(envelope["stdout"], "observed stdout");
            assert_eq!(envelope["stderr"], "observed stderr");
            assert!(envelope["exit_code"].is_null());
            assert_eq!(envelope["data"], serde_json::json!({"observed": 1}));
            assert!(envelope["error"].as_str().unwrap().contains("restarted"));
        }
        let notice = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let result = actor.execute_kj_quiet(context, vec!["wait".into(), "--operation".into(), "interrupted".into(), "--timeout".into(), "0".into()]).await.unwrap();
                let data = result.data.unwrap();
                let notice = &data["state"]["notifications"][0];
                if notice["status"] == "delivered" { break notice.clone(); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("recovered completion is delivered without a new ledger event");
        assert_eq!(notice["resume_allowed"], false);
        let id = kaijutsu_types::BlockId::from_key(notice["block_id"].as_str().unwrap()).unwrap();
        let delivered = actor.get_block(context, id).await.unwrap().unwrap();
        assert!(delivered.content.contains("observed stdout"));
        assert!(delivered.content.contains("observed stderr"));
        assert!(delivered.content.contains("restarted"));
        assert!(!marker.exists(), "startup must not execute interrupted source");
        drop(actor);
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
        server.abort();
        let _ = server.await;
    });
}

#[test]
fn job_completion_reports_execution_while_operation_publication_is_pending() {
    common::run_local(async {
        for retention_fault in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        let key = config.root_key();
        let path = config.data_dir.as_ref().unwrap().join("kernel.db");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::task::spawn_local(async move {
            SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
        });
        let shared = rx.await.unwrap();
        let client = kaijutsu_client::connect_ssh(SshConfig {
            host: addr.ip().to_string(), port: addr.port(), username: "amy".into(),
            key_source: KeySource::InMemory(key), insecure: true,
        }).await.unwrap();
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = common::create_context(&kj, "job-publication").await.unwrap();
        let observer = common::create_context(&kj, "publication-observer").await.unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(if retention_fault {
            "CREATE TRIGGER fail_receipt BEFORE INSERT ON shell_operation_outcomes BEGIN SELECT RAISE(ABORT, 'injected outcome retention fault'); END;"
        } else {
            "CREATE TRIGGER fail_receipt BEFORE UPDATE OF completed_at ON shell_operations BEGIN SELECT RAISE(ABORT, 'injected receipt fault'); END;"
        }).unwrap();
        let command = kj.shell_execute("echo observed; echo warning >&2", context, false).await.unwrap();
        let operation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let operation = shared.kernel.shell_operations().list_for_context(context).unwrap().into_iter().find(|op| op.receipt.command_block_id == command);
                if let Some(operation) = operation {
                    if let Some(job) = &operation.receipt.job_id {
                        let result = kj.execute_kj_quiet(observer, &[
                            "wait".into(), "--job".into(), job.clone(), "--timeout".into(), "0".into(), context.to_hex(),
                        ]).await.unwrap();
                        assert_eq!(result.exit_code, 0, "{}", result.stderr);
                        let data = result.data.unwrap();
                        if data["timed_out"] == false {
                            assert_eq!(data["state"]["exit_code"], 0);
                            break operation;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("job settles even if durable projection fails");
        let result = kj.execute_kj_quiet(observer, &[
            "wait".into(), "--operation".into(), operation.receipt.operation_id.clone(),
            "--timeout".into(), "0".into(), context.to_hex(),
        ]).await.unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        let data = result.data.unwrap();
        assert_eq!(data["timed_out"], true);
        if retention_fault {
            assert!(data["state"]["retention_error"].as_str().unwrap().contains("injected outcome retention fault"));
            assert!(shared.kernel.shell_operations().outcome(&operation.receipt.operation_id, context).unwrap().is_none());
            conn.execute_batch("DROP TRIGGER fail_receipt").unwrap();
            let recovered = kj.execute_kj_quiet(observer, &[
                "wait".into(), "--operation".into(), operation.receipt.operation_id.clone(),
                "--timeout".into(), "5".into(), context.to_hex(),
            ]).await.unwrap();
            assert_eq!(recovered.exit_code, 0, "{}", recovered.stderr);
            let data = recovered.data.unwrap();
            assert_eq!(data["timed_out"], false);
            assert!(data["state"]["retention_error"].is_null());
            assert_eq!(data["state"]["envelope"]["stdout"], "observed\n");
        }
        let retained = shared.kernel.shell_operations().outcome(&operation.receipt.operation_id, context).unwrap().unwrap();
        assert_eq!(retained.envelope().exit_code, Some(0));
        assert_eq!(retained.envelope().stdout, "observed\n");
        assert_eq!(retained.envelope().stderr, "warning\n");
        if !retention_fault { conn.execute_batch("DROP TRIGGER fail_receipt").unwrap(); }
        drop(kj);
        drop(client);
        shared.kernel.shutdown_runtime_worker().await.unwrap();
        server.abort();
        let _ = server.await;
        }
    });
}
