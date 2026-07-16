//! Standalone SFTP fetch probe against a live kernel — the genuine-SSH
//! subsystem-dispatch check the in-process duplex e2e deliberately can't
//! cover (`docs/slash-r.md`). Usage:
//!
//! ```sh
//! cargo run -p kaijutsu-client --example sftp_probe -- <hash> [host] [port]
//! ```
//!
//! Connects with agent auth as the current user, opens the `sftp` subsystem,
//! reads `/v/cas/<ab>/<hash>`, and prints the byte count. Every stage is
//! timed and printed so a hang names its own stage.

use kaijutsu_client::{SftpClient, SshConfig};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = std::env::args().skip(1);
    let hash: kaijutsu_cas::ContentHash = args
        .next()
        .expect("usage: sftp_probe <hash> [host] [port]")
        .parse()?;
    let mut config = SshConfig::default();
    if let Some(h) = args.next() {
        config.host = h;
    }
    if let Some(p) = args.next() {
        config.port = p.parse()?;
    }

    let t = Instant::now();
    eprintln!("[probe] connecting to {}:{} …", config.host, config.port);
    let client = SftpClient::connect(config).await?;
    eprintln!("[probe] connected + sftp session ready in {:?}", t.elapsed());

    let t = Instant::now();
    eprintln!("[probe] reading object {hash} …");
    let bytes = client.read_object(&hash).await?;
    eprintln!("[probe] read {} bytes in {:?}", bytes.len(), t.elapsed());
    Ok(())
}
