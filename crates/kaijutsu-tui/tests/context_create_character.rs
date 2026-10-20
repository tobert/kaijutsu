//! A root character performs only its root console, so a root's session
//! names the character that plays the contexts it creates (`--character`).
//! Each test stands up an ephemeral kernel and drives `KernelBridge::open`,
//! the path `kaijutsu-tui --context <label>` takes.

mod support;

use std::time::Duration;

use kaijutsu_client::{KeySource, SshConfig};
use kaijutsu_server::SshServerConfig;
use kaijutsu_tui::bridge::KernelBridge;
use support::{EphemeralServer, serial};

/// Run `with` against a bridge connected as the ephemeral kernel's root
/// character, created with `character` as its `--character`.
fn with_bridge<T>(
    server: &EphemeralServer,
    character: Option<&str>,
    with: impl AsyncFnOnce(KernelBridge) -> T,
) -> T {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("build runtime");
    let _enter = rt.enter();
    let local = tokio::task::LocalSet::new();
    let out = local.block_on(&rt, async {
        let config = SshConfig {
            host: server.addr.ip().to_string(),
            port: server.addr.port(),
            username: "probe".into(),
            key_source: KeySource::InMemory(server.root_key.clone()),
            insecure: true,
        };
        let bridge = KernelBridge::connect(config, "coder".into(), None, character.map(String::from), Duration::from_secs(10))
            .await
            .expect("connect the bridge");
        with(bridge).await
    });
    drop(local);
    out
}

#[test]
fn a_root_session_without_a_character_refuses_to_create_and_names_the_option() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let error = with_bridge(&server, None, async |bridge| bridge.open("lane").await.expect_err("a root cannot perform a context"));
    let message = format!("{error:#}");
    assert!(message.contains("--character"), "the refusal names the option: {message}");
    assert!(message.contains(SshServerConfig::EPHEMERAL_ROOT), "and the root: {message}");
}

#[test]
fn a_root_session_with_a_character_creates_a_context_it_plays() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let info = with_bridge(&server, Some(SshServerConfig::EPHEMERAL_PERFORMER), async |bridge| {
        let info = bridge.open("lane").await.expect("a named character plays the context");
        let result = bridge
            .actor()
            .execute_kj_quiet(info.id, vec!["context".into(), "info".into(), info.id.to_hex()])
            .await
            .expect("run kj context info");
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        result.data.as_ref().and_then(|d| d.get("played_by_name")).and_then(|n| n.as_str()).map(String::from)
    });
    assert_eq!(info.as_deref(), Some(SshServerConfig::EPHEMERAL_PERFORMER));
}
