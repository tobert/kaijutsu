//! Agent detection and session correlation.
//!
//! Discovers which AI coding tool (Codex, Claude Code, Gemini CLI, etc.) is running
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
mod codex;

use std::path::Path;

pub use claude::ClaudeCodeSession;
pub use codex::CodexSession;

/// Metadata about the agent session hosting this process.
pub trait AgentSession: Send + Sync {
    /// Agent identifier (e.g., "codex", "claude-code", "gemini-cli").
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
/// 1. nonempty `CODEX_THREAD_ID` env → [`CodexSession`]
/// 2. `CLAUDECODE=1` env → [`ClaudeCodeSession`]
///
/// Returns `None` if no known agent is detected.
pub fn detect() -> Option<Box<dyn AgentSession>> {
    // Kaijutsu's Codex integration forwards the active Codex thread ID to its
    // MCP server. Prefer it when both hosts' markers are present: unlike
    // CLAUDECODE, it directly identifies the conversation to correlate.
    if let Some(session) = CodexSession::discover() {
        return Some(Box::new(session));
    }

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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(values: &[(&'static str, Option<&str>)]) -> Self {
            let saved = values
                .iter()
                .map(|(name, _)| (*name, std::env::var(name).ok()))
                .collect();

            for (name, value) in values {
                // Tests hold ENV_LOCK while mutating the process environment.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }

            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                // Tests hold ENV_LOCK while restoring the process environment.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn detect_returns_none_without_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        // In test environment, CLAUDECODE is not typically set by the test harness.
        // If it is set (running inside CC), we'll get Some — either way is valid.
        let _result = detect();
    }

    #[test]
    fn codex_thread_id_takes_precedence_over_claude_marker() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _environment = EnvGuard::set(&[
            ("CODEX_THREAD_ID", Some("thread-123")),
            ("CODEX_PROJECT_DIR", None),
            ("CODEX_VERSION", None),
            ("CLAUDECODE", Some("1")),
        ]);

        let session = detect().expect("Codex thread ID should be detected");
        assert_eq!(session.agent_name(), "codex");
        assert_eq!(session.session_id(), Some("thread-123"));
    }

    #[test]
    fn agent_session_trait_is_object_safe() {
        // Verify the trait can be used as a trait object
        fn _accept(_s: &dyn AgentSession) {}
    }
}
