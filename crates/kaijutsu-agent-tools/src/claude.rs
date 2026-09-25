//! Claude Code session detection.
//!
//! Claude Code hands each MCP server it spawns the active session's id in
//! `CLAUDE_CODE_SESSION_ID`, and its project directory in
//! `CLAUDE_PROJECT_DIR`. That id is the session correlation key for the
//! server's lifetime, until `/clear` starts a new one (a hook event
//! announces it).

use std::path::{Path, PathBuf};

use crate::AgentSession;

/// Claude Code session metadata supplied by the host environment.
#[derive(Debug, Clone)]
pub struct ClaudeCodeSession {
    session_id: Option<String>,
    project_dir: Option<PathBuf>,
}

impl ClaudeCodeSession {
    /// Discover the Claude Code session this process was spawned for.
    ///
    /// Without `CLAUDE_CODE_SESSION_ID` the session id stays unknown, and the
    /// first hook event names it. When `CLAUDE_PROJECT_DIR` is absent, the
    /// current directory is the project directory.
    pub fn discover() -> Self {
        Self::from_env(|name| std::env::var(name).ok(), std::env::current_dir().ok())
    }

    fn from_env(get: impl Fn(&str) -> Option<String>, current_dir: Option<PathBuf>) -> Self {
        let session_id = nonempty(get("CLAUDE_CODE_SESSION_ID"));
        if session_id.is_none() {
            tracing::warn!(
                "CLAUDECODE=1 but CLAUDE_CODE_SESSION_ID is unset; the first hook event \
                 will name the session"
            );
        }
        Self {
            session_id,
            project_dir: nonempty(get("CLAUDE_PROJECT_DIR")).map(PathBuf::from).or(current_dir),
        }
    }
}

impl AgentSession for ClaudeCodeSession {
    fn agent_name(&self) -> &str {
        "claude-code"
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn project_dir(&self) -> Option<&Path> {
        self.project_dir.as_deref()
    }

    fn version(&self) -> Option<&str> {
        None
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_names_the_session_and_project() {
        let session = ClaudeCodeSession::from_env(
            |name| match name {
                "CLAUDE_CODE_SESSION_ID" => Some("70b2c659-f80b-43ad-9f85-53196913838f".into()),
                "CLAUDE_PROJECT_DIR" => Some("/home/amy/src/kaijutsu".into()),
                _ => None,
            },
            Some(PathBuf::from("/fallback")),
        );

        assert_eq!(session.agent_name(), "claude-code");
        assert_eq!(session.session_id(), Some("70b2c659-f80b-43ad-9f85-53196913838f"));
        assert_eq!(session.project_dir(), Some(Path::new("/home/amy/src/kaijutsu")));
    }

    #[test]
    fn without_a_host_session_id_the_session_is_unknown() {
        let session = ClaudeCodeSession::from_env(
            |name| (name == "CLAUDE_CODE_SESSION_ID").then(|| "  ".into()),
            Some(PathBuf::from("/home/amy/src/kaijutsu")),
        );

        assert_eq!(session.session_id(), None);
        assert_eq!(session.project_dir(), Some(Path::new("/home/amy/src/kaijutsu")));
    }
}
