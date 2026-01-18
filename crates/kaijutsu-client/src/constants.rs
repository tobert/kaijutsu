//! Client configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.

use std::time::Duration;

/// Default SSH host for local development.
pub const DEFAULT_SSH_HOST: &str = "localhost";

/// Default SSH port.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// SSH inactivity timeout.
pub const SSH_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(300);

/// SSH keep-alive interval.
pub const SSH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// SSH keep-alive max retries.
pub const SSH_KEEPALIVE_MAX: usize = 3;
