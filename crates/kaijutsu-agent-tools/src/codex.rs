//! Codex session detection.
//!
//! Codex does not expose its active thread through a transcript file that an
//! MCP server can reliably correlate.  Instead, integrations forward
//! `CODEX_THREAD_ID` into the server environment.  That ID is the stable
//! session correlation key for the server's lifetime.

use std::path::{Path, PathBuf};

use crate::AgentSession;

/// Codex session metadata supplied by the host environment.
#[derive(Debug, Clone)]
pub struct CodexSession {
    thread_id: String,
    project_dir: Option<PathBuf>,
    version: Option<String>,
}

impl CodexSession {
    /// Discover the Codex session forwarded to this process.
    ///
    /// `CODEX_THREAD_ID` is required.  Integrations may additionally forward
    /// `CODEX_PROJECT_DIR` and `CODEX_VERSION`; when they do not, the current
    /// directory remains useful project metadata.
    pub fn discover() -> Option<Self> {
        Self::from_env(|name| std::env::var(name).ok(), std::env::current_dir().ok())
    }

    fn from_env(
        get: impl Fn(&str) -> Option<String>,
        current_dir: Option<PathBuf>,
    ) -> Option<Self> {
        let thread_id = nonempty(get("CODEX_THREAD_ID"))?;
        let project_dir = nonempty(get("CODEX_PROJECT_DIR"))
            .map(PathBuf::from)
            .or(current_dir);

        Some(Self {
            thread_id,
            project_dir,
            version: nonempty(get("CODEX_VERSION")),
        })
    }
}

impl AgentSession for CodexSession {
    fn agent_name(&self) -> &str {
        "codex"
    }

    fn session_id(&self) -> Option<&str> {
        Some(&self.thread_id)
    }

    fn slug(&self) -> Option<&str> {
        None
    }

    fn project_dir(&self) -> Option<&Path> {
        self.project_dir.as_deref()
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_a_nonempty_thread_id() {
        assert!(CodexSession::from_env(|_| None, None).is_none());
        assert!(CodexSession::from_env(|_| Some("  ".into()), None).is_none());
    }

    #[test]
    fn reads_thread_and_optional_metadata() {
        let session = CodexSession::from_env(
            |name| match name {
                "CODEX_THREAD_ID" => Some("thread-123".into()),
                "CODEX_PROJECT_DIR" => Some("/workspace/project".into()),
                "CODEX_VERSION" => Some("0.114.0".into()),
                _ => None,
            },
            Some(PathBuf::from("/fallback")),
        )
        .unwrap();

        assert_eq!(session.agent_name(), "codex");
        assert_eq!(session.session_id(), Some("thread-123"));
        assert_eq!(session.slug(), None);
        assert_eq!(session.project_dir(), Some(Path::new("/workspace/project")));
        assert_eq!(session.version(), Some("0.114.0"));
    }

    #[test]
    fn uses_current_directory_when_project_directory_is_not_forwarded() {
        let session = CodexSession::from_env(
            |name| (name == "CODEX_THREAD_ID").then(|| "thread-123".into()),
            Some(PathBuf::from("/workspace/project")),
        )
        .unwrap();

        assert_eq!(session.project_dir(), Some(Path::new("/workspace/project")));
        assert_eq!(session.version(), None);
    }
}
