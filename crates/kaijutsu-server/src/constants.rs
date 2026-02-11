//! Server configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.

use std::time::Duration;

/// Default SSH port for kaijutsu server.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// Default bind address (localhost only for security).
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1";

/// SSH authentication rejection delay (prevents timing attacks).
pub const SSH_AUTH_REJECTION_DELAY: Duration = Duration::from_secs(1);
