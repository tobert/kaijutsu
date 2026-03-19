//! Shared helpers for kaijutsu-server integration tests.
//!
//! Provides `run_local`, `start_server`, `connect_client`, and utilities
//! for exercising the full SSH + Cap'n Proto stack.

use std::net::SocketAddr;

use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, RpcClient, SshConfig};
use kaijutsu_server::{SshServer, SshServerConfig};

/// Run async test code on a single-threaded runtime with LocalSet (capnp-rpc requirement).
pub fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Start an SSH server on an ephemeral port and return the address.
///
/// The listener is pre-bound so connections queue during kernel initialization.
pub async fn start_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Start a server whose ephemeral config dir contains a `models.rhai` with a mock provider.
///
/// This makes `initialize_kernel_models()` register a "mock" provider so that
/// `KjDispatcher.summarize()` and other LLM-dependent paths work in tests.
pub async fn start_server_with_mock_llm() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let config = SshServerConfig::ephemeral(addr.port());

    // Write a models.rhai that uses the mock provider into the ephemeral config dir
    if let Some(ref config_dir) = config.config_dir {
        let models_rhai = r#"
let config = #{
    default_provider: "mock",
    providers: [
        #{
            provider_type: "mock",
            enabled: true,
            default_model: "mock-model",
        },
    ],
    model_aliases: #{},
};
config
"#;
        std::fs::write(config_dir.join("models.rhai"), models_rhai)
            .expect("failed to write models.rhai to ephemeral config dir");
    }

    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run_on_listener(listener).await {
            log::error!("Server error: {}", e);
        }
    });

    tokio::task::yield_now().await;
    addr
}

/// Connect to server with ephemeral key.
pub async fn connect_client(addr: SocketAddr) -> RpcClient {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    };

    let mut ssh_client = kaijutsu_client::SshClient::new(config);
    let channels = ssh_client.connect().await.expect("SSH connect failed");
    RpcClient::new(channels.rpc.into_stream())
        .await
        .expect("RPC client init failed")
}
