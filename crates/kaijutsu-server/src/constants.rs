//! Server configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.

use std::time::Duration;

/// Default SSH port for kaijutsu server.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// Default TCP port for Cap'n Proto RPC.
pub const DEFAULT_TCP_PORT: u16 = 7878;

/// Default bind address (localhost only for security).
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1";

/// SSH authentication rejection delay (prevents timing attacks).
pub const SSH_AUTH_REJECTION_DELAY: Duration = Duration::from_secs(1);

/// kaish socket connection timeout.
pub const KAISH_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// kaish shutdown wait time.
pub const KAISH_SHUTDOWN_WAIT: Duration = Duration::from_millis(100);

/// kaish socket retry interval.
pub const KAISH_SOCKET_RETRY_INTERVAL: Duration = Duration::from_millis(50);
