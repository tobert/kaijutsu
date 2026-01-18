//! Kaish subprocess integration for kaijutsu-server.
//!
//! Spawns kaish as a separate process and communicates via Unix socket + Cap'n Proto.
//! This provides process isolation, clean API boundary, and crash isolation.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tokio::time::sleep;

use crate::constants::{KAISH_SHUTDOWN_WAIT, KAISH_SOCKET_RETRY_INTERVAL, KAISH_SOCKET_TIMEOUT};

use kaish_client::{ClientError, IpcClient, KernelClient};
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::state::paths::runtime_dir;

/// A kaish kernel running as a subprocess.
///
/// Manages the lifecycle of a kaish process and provides IPC communication.
pub struct KaishProcess {
    /// The subprocess handle
    child: Child,
    /// IPC client connected to the subprocess
    client: IpcClient,
    /// Socket path for this kernel
    socket_path: PathBuf,
    /// Kernel name/id
    name: String,
}

impl KaishProcess {
    /// Spawn a new kaish subprocess for the given kernel.
    ///
    /// Creates a socket at `$XDG_RUNTIME_DIR/kaish/<name>.sock` and starts
    /// `kaish serve --socket=<path> --name=<name>`.
    pub async fn spawn(name: &str) -> Result<Self> {
        let socket_dir = runtime_dir();
        let socket_path = socket_dir.join(format!("{}.sock", name));

        // Ensure the socket directory exists
        tokio::fs::create_dir_all(&socket_dir)
            .await
            .with_context(|| format!("failed to create socket directory: {}", socket_dir.display()))?;

        // Remove stale socket if it exists
        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path)
                .await
                .with_context(|| format!("failed to remove stale socket: {}", socket_path.display()))?;
        }

        // Find the kaish binary
        let kaish_bin = find_kaish_binary()?;

        log::info!(
            "Spawning kaish subprocess: {} serve --socket={} --name={}",
            kaish_bin.display(),
            socket_path.display(),
            name
        );

        // Spawn the kaish process
        let child = Command::new(&kaish_bin)
            .arg("serve")
            .arg(format!("--socket={}", socket_path.display()))
            .arg(format!("--name={}", name))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn kaish: {}", kaish_bin.display()))?;

        // Wait for the socket to appear (with timeout)
        let client = wait_for_socket(&socket_path, KAISH_SOCKET_TIMEOUT).await?;

        log::info!("Connected to kaish subprocess at {}", socket_path.display());

        Ok(Self {
            child,
            client,
            socket_path,
            name: name.to_string(),
        })
    }

    /// Execute kaish code and return the result.
    pub async fn execute(&self, code: &str) -> Result<ExecResult> {
        self.client.execute(code).await.map_err(|e| e.into())
    }

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Result<Option<kaish_kernel::ast::Value>> {
        self.client.get_var(name).await.map_err(|e| e.into())
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: &str, value: kaish_kernel::ast::Value) -> Result<()> {
        self.client.set_var(name, value).await.map_err(|e| e.into())
    }

    /// List all variable names.
    pub async fn list_vars(&self) -> Result<Vec<String>> {
        let vars = self.client.list_vars().await?;
        Ok(vars.into_iter().map(|(name, _)| name).collect())
    }

    /// Ping the kernel (health check).
    pub async fn ping(&self) -> Result<String> {
        self.client.ping().await.map_err(|e| e.into())
    }

    /// Get the socket path for this kernel.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Get the kernel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Shutdown the kaish subprocess gracefully.
    pub async fn shutdown(mut self) -> Result<()> {
        // Try graceful shutdown first
        if let Err(e) = self.client.shutdown().await {
            log::warn!("Graceful shutdown failed: {}, killing process", e);
        }

        // Wait a bit for graceful exit
        sleep(KAISH_SHUTDOWN_WAIT).await;

        // Kill if still running
        match self.child.try_wait() {
            Ok(Some(status)) => {
                log::debug!("kaish exited with status: {}", status);
            }
            Ok(None) => {
                log::warn!("kaish still running, sending SIGKILL");
                self.child.kill().await?;
            }
            Err(e) => {
                log::error!("Failed to check kaish status: {}", e);
            }
        }

        // Clean up socket
        if self.socket_path.exists() {
            let _ = tokio::fs::remove_file(&self.socket_path).await;
        }

        Ok(())
    }
}

impl Drop for KaishProcess {
    fn drop(&mut self) {
        // Note: kill_on_drop(true) handles process cleanup
        // Socket cleanup happens in shutdown() or will be cleaned on next spawn
    }
}

/// Find the kaish binary.
///
/// Searches in order:
/// 1. KAISH_BIN environment variable
/// 2. cargo target directory (for development)
/// 3. PATH
fn find_kaish_binary() -> Result<PathBuf> {
    // Check environment variable first
    if let Ok(bin) = std::env::var("KAISH_BIN") {
        let path = PathBuf::from(&bin);
        if path.exists() {
            return Ok(path);
        }
        log::warn!("KAISH_BIN set to {} but file not found", bin);
    }

    // Check cargo target directory (development)
    // Assuming kaish and kaijutsu repos are siblings
    let dev_paths = [
        // Relative to kaijutsu repo
        "../kaish/target/debug/kaish",
        "../kaish/target/release/kaish",
        // Absolute common locations
        "~/.cargo/bin/kaish",
    ];

    for path in dev_paths {
        let expanded = shellexpand::tilde(path);
        let path = PathBuf::from(expanded.as_ref());
        if path.exists() {
            log::debug!("Found kaish at {}", path.display());
            return Ok(path);
        }
    }

    // Check PATH
    if let Ok(output) = std::process::Command::new("which").arg("kaish").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    anyhow::bail!(
        "kaish binary not found. Set KAISH_BIN environment variable or ensure kaish is in PATH."
    )
}

/// Wait for a socket to appear and connect to it.
async fn wait_for_socket(socket_path: &PathBuf, timeout: Duration) -> Result<IpcClient> {
    let start = std::time::Instant::now();
    let retry_interval = KAISH_SOCKET_RETRY_INTERVAL;

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timeout waiting for kaish socket at {}",
                socket_path.display()
            );
        }

        if socket_path.exists() {
            // Socket file exists, try to connect
            match IpcClient::connect(socket_path).await {
                Ok(client) => {
                    // Verify connection with a ping
                    match client.ping().await {
                        Ok(_) => return Ok(client),
                        Err(ClientError::Connection(_)) => {
                            // Socket exists but not ready yet
                            sleep(retry_interval).await;
                            continue;
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
                Err(ClientError::Connection(_)) => {
                    // Socket not ready yet
                    sleep(retry_interval).await;
                    continue;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        } else {
            sleep(retry_interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Requires kaish binary to be built
    async fn spawn_and_execute() {
        let process = KaishProcess::spawn("test-spawn").await.unwrap();
        let result = process.execute("echo hello").await.unwrap();
        assert!(result.ok());
        assert_eq!(result.out.trim(), "hello");
        process.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore] // Requires kaish binary to be built
    async fn spawn_and_ping() {
        let process = KaishProcess::spawn("test-ping").await.unwrap();
        let pong = process.ping().await.unwrap();
        assert_eq!(pong, "pong");
        process.shutdown().await.unwrap();
    }
}
