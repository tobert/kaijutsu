//! Drive the real `kjc` binary against a real kernel: an ephemeral server
//! on a loopback port, reached with the root key it minted.
use std::path::Path;
use std::process::{Command, Output};

use kaijutsu_server::{SshServer, SshServerConfig};

/// A kernel serving on a loopback port for the life of the test process.
fn start_kernel(key_path: &Path) -> u16 {
    let config = SshServerConfig::ephemeral(0);
    config.root_key().write_openssh_file(key_path, Default::default()).expect("write the root key");
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        rt.block_on(local.run_until(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            let _ = SshServer::new(config).run_on_listener(listener).await;
        }));
    });
    port_rx.recv().expect("the kernel thread reports its port")
}

fn kjc(port: u16, key: &Path, context: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kjc"))
        .args(["--insecure", "--host", "127.0.0.1", "--port", &port.to_string(), "--key-file"])
        .arg(key)
        .args(["--context", context])
        .args(args)
        .env_remove("KJC_CONTEXT")
        .env_remove("KAIJUTSU_KEY_FILE")
        .env_remove("KAIJUTSU_KEY_FINGERPRINT")
        .output()
        .expect("run kjc")
}

fn text(bytes: &[u8]) -> String { String::from_utf8_lossy(bytes).into_owned() }

#[test]
fn kjc_runs_kj_verbs_and_shell_commands_and_returns_their_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key");
    let port = start_kernel(&key);
    let root = SshServerConfig::EPHEMERAL_ROOT;

    let created = kjc(port, &key, root, &["kj", "context", "create", "verify"]);
    assert!(created.status.success(), "create: {}{}", text(&created.stdout), text(&created.stderr));

    let listed = kjc(port, &key, "verify", &["kj", "block", "list", "--tail", "1"]);
    assert!(listed.status.success(), "list: {}", text(&listed.stderr));
    assert!(text(&listed.stdout).contains("showing the last 1 of"), "{}", text(&listed.stdout));

    let echoed = kjc(port, &key, "verify", &["sh", "echo hello from kjc"]);
    assert_eq!(echoed.status.code(), Some(0), "{}", text(&echoed.stderr));
    assert_eq!(text(&echoed.stdout), "hello from kjc\n");

    let failed = kjc(port, &key, "verify", &["sh", "false"]);
    assert_eq!(failed.status.code(), Some(1), "a failing command's exit code comes back");

    let missing = kjc(port, &key, "no-such-context", &["kj", "context", "list"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("kj context create no-such-context"), "{}", text(&missing.stderr));
}
