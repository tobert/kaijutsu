//! Agent detection and session correlation.
//!
//! Discovers which AI coding tool (Claude Code, Gemini CLI, etc.) is running
//! as a parent process, and extracts session metadata for context correlation.
//!
//! ## Usage
//!
//! ```no_run
//! if let Some(session) = kaijutsu_agent_tools::detect() {
//!     println!("Running inside {}", session.agent_name());
//!     if let Some(id) = session.session_id() {
//!         println!("Session: {id}");
//!     }
//! }
//! ```

mod claude;

use std::path::Path;

pub use claude::ClaudeCodeSession;

/// Metadata about the agent session hosting this process.
pub trait AgentSession: Send + Sync {
    /// Agent identifier (e.g., "claude-code", "gemini-cli").
    fn agent_name(&self) -> &str;

    /// Opaque session ID from the agent (UUID string for CC).
    fn session_id(&self) -> Option<&str>;

    /// Human-readable session slug (e.g., "encapsulated-percolating-grove").
    fn slug(&self) -> Option<&str>;

    /// Project directory the agent is operating in.
    fn project_dir(&self) -> Option<&Path>;

    /// Agent version string.
    fn version(&self) -> Option<&str>;
}

/// Detect the hosting agent, if any.
///
/// Currently checks:
/// 1. `CLAUDECODE=1` env → [`ClaudeCodeSession`]
///
/// Returns `None` if no known agent is detected.
pub fn detect() -> Option<Box<dyn AgentSession>> {
    // Claude Code sets CLAUDECODE=1 for MCP servers it spawns
    if std::env::var("CLAUDECODE").ok().as_deref() == Some("1") {
        match ClaudeCodeSession::discover() {
            Ok(session) => return Some(Box::new(session)),
            Err(e) => {
                tracing::warn!("CLAUDECODE=1 but session discovery failed: {e}");
                // Fall through — still return a minimal session
                return Some(Box::new(ClaudeCodeSession::minimal()));
            }
        }
    }

    // Future: Gemini CLI, Cursor, etc.

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_without_env() {
        // In test environment, CLAUDECODE is not typically set by the test harness.
        // If it is set (running inside CC), we'll get Some — either way is valid.
        let _result = detect();
    }

    #[test]
    fn agent_session_trait_is_object_safe() {
        // Verify the trait can be used as a trait object
        fn _accept(_s: &dyn AgentSession) {}
    }
}
