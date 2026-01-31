//! Kaijutsu server binary
//!
//! SSH + Cap'n Proto RPC server for kaijutsu.

use std::env;

use kaijutsu_server::constants::DEFAULT_SSH_PORT;
use kaijutsu_server::{SshServer, SshServerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    let port = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_SSH_PORT);

    log::info!("Starting kaijutsu server on SSH port {}...", port);

    let config = SshServerConfig::ephemeral(port);
    let server = SshServer::new(config);
    server.run().await?;

    Ok(())
}
