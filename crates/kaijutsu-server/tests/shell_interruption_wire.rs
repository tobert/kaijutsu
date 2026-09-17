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
        let (context, command, output) = {
            let shared = kaijutsu_server::rpc::create_shared_kernel(
                config.config_dir.as_deref(), &config.config_mounts, config.data_dir.as_deref(),
            ).await.unwrap();
            let amy = shared.kernel_db.lock().get_character_by_name("amy").unwrap().unwrap().principal_id;
            let context = shared.kernel_db.lock().get_character(amy).unwrap().unwrap().root_ctx.unwrap();
            let blocks = &shared.documents;
            let command = blocks.insert_tool_call(context, None, None, "shell", serde_json::json!({}), None).unwrap();
            let output = blocks.insert_tool_result(context, &command, Some(&command), "observed stdout", false, None, None).unwrap();
            for id in [&command, &output] { blocks.set_status(context, id, Status::Running).unwrap(); }
            blocks.set_stderr(context, &output, Some("observed stderr".into())).unwrap();
            blocks.set_output(context, &output, Some(&OutputData::new().with_rich_json(serde_json::json!({"observed": 1})))).unwrap();
            // Persist the state of a writer that departed before recording its outcome.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("INSERT INTO shell_operations(operation_id,context_id,principal_id,actor_id,command_block_id,output_block_id,source,created_at)
                VALUES('interrupted',?1,?2,?2,?3,?4,?5,1)", rusqlite::params![
                context.as_bytes().to_vec(), amy.as_bytes().to_vec(), command.to_key(), output.to_key(),
                format!("echo unexpected > '{}'", marker.display()),
            ]).unwrap();
            shared.kernel.shutdown_runtime_worker().await.unwrap();
            (context, command, output)
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
        for id in [command, output] {
            assert_eq!(actor.get_block(context, id).await.unwrap().unwrap().status, Status::Error);
        }
        let result = actor.get_block(context, output).await.unwrap().unwrap();
        assert_eq!(result.content, "observed stdout");
        assert!(result.is_error);
        assert_eq!(result.exit_code, None);
        assert!(result.stderr.as_deref().unwrap().starts_with("observed stderr\n"));
        assert!(result.stderr.as_deref().unwrap().contains("restarted"));
        assert_eq!(result.output.unwrap().to_json(), serde_json::json!({"observed": 1}));
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
        assert!(!marker.exists(), "startup must not execute interrupted source");
        drop(actor);
        kernel.kernel.shutdown_runtime_worker().await.unwrap();
        server.abort();
        let _ = server.await;
    });
}
