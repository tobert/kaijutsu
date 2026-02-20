//! Claude Code session detection.
//!
//! Discovers the active Claude Code session by scanning JSONL transcript files
//! in `~/.claude/projects/{encoded-path}/`. The filename (minus `.jsonl`) is
//! the session UUID, and the first few lines contain session metadata.

use std::path::{Path, PathBuf};

use crate::AgentSession;

/// Claude Code session metadata.
#[derive(Debug, Clone)]
pub struct ClaudeCodeSession {
    /// Session UUID (from JSONL filename).
    session_id: Option<String>,
    /// Human-readable slug (e.g., "encapsulated-percolating-grove").
    slug: Option<String>,
    /// Project directory Claude Code is operating in.
    project_dir: Option<PathBuf>,
    /// Claude Code version.
    version: Option<String>,
    /// Parent process ID (for hook socket correlation).
    ppid: u32,
}

impl ClaudeCodeSession {
    /// Discover the current Claude Code session.
    ///
    /// 1. Encode `cwd` the way Claude does (absolute path → dash-separated)
    /// 2. Scan `~/.claude/projects/{encoded}/*.jsonl` for the most recent file
    /// 3. Parse the filename as session UUID
    /// 4. Read first few lines for slug, version, cwd
    pub fn discover() -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("Cannot get cwd: {e}"))?;

        let home = dirs::home_dir()
            .ok_or("Cannot determine home directory")?;

        let encoded = encode_project_path(&cwd);
        let projects_dir = home.join(".claude").join("projects").join(&encoded);

        if !projects_dir.is_dir() {
            return Err(format!(
                "Claude projects dir not found: {}",
                projects_dir.display()
            ));
        }

        // Find the most recently modified JSONL file
        let jsonl = most_recent_jsonl(&projects_dir)?;
        let session_id = jsonl
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        // Parse metadata from first few lines
        let meta = parse_session_metadata(&jsonl);

        let ppid = std::os::unix::process::parent_id();

        Ok(Self {
            session_id,
            slug: meta.slug,
            project_dir: meta.cwd.map(PathBuf::from),
            version: meta.version,
            ppid,
        })
    }

    /// Create a minimal session when discovery fails but CLAUDECODE=1 is set.
    pub fn minimal() -> Self {
        Self {
            session_id: None,
            slug: None,
            project_dir: std::env::current_dir().ok(),
            version: None,
            ppid: std::os::unix::process::parent_id(),
        }
    }

    /// Parent process ID (Claude Code is typically the direct parent).
    pub fn ppid(&self) -> u32 {
        self.ppid
    }
}

impl AgentSession for ClaudeCodeSession {
    fn agent_name(&self) -> &str {
        "claude-code"
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn slug(&self) -> Option<&str> {
        self.slug.as_deref()
    }

    fn project_dir(&self) -> Option<&Path> {
        self.project_dir.as_deref()
    }

    fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

/// Encode an absolute path the way Claude Code does for project directories.
///
/// `/home/atobey/src/kaijutsu` → `-home-atobey-src-kaijutsu`
///
/// Each path separator becomes a `-`, and the leading `/` becomes a leading `-`.
pub fn encode_project_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.replace('/', "-")
}

/// Find the most recently modified `.jsonl` file in a directory.
fn most_recent_jsonl(dir: &Path) -> Result<PathBuf, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;

    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);

        if best.as_ref().map_or(true, |(_, prev)| mtime > *prev) {
            best = Some((path, mtime));
        }
    }

    best.map(|(p, _)| p)
        .ok_or_else(|| format!("No .jsonl files in {}", dir.display()))
}

/// Metadata parsed from the first few lines of a session JSONL.
#[derive(Default)]
struct SessionMeta {
    slug: Option<String>,
    version: Option<String>,
    cwd: Option<String>,
}

/// Parse session metadata from the first few JSONL lines.
///
/// CC transcript lines include `slug`, `version`, `cwd`, `sessionId` etc.
/// We only need to find one line with these fields — typically the first
/// `"type": "user"` entry.
fn parse_session_metadata(path: &Path) -> SessionMeta {
    use std::io::{BufRead, BufReader};

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return SessionMeta::default(),
    };

    let reader = BufReader::new(file);
    let mut meta = SessionMeta::default();

    // Check up to 5 lines — metadata appears early
    for line in reader.lines().take(5).flatten() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };

        if meta.slug.is_none() {
            if let Some(s) = val.get("slug").and_then(|v| v.as_str()) {
                meta.slug = Some(s.to_string());
            }
        }
        if meta.version.is_none() {
            if let Some(v) = val.get("version").and_then(|v| v.as_str()) {
                meta.version = Some(v.to_string());
            }
        }
        if meta.cwd.is_none() {
            if let Some(c) = val.get("cwd").and_then(|v| v.as_str()) {
                meta.cwd = Some(c.to_string());
            }
        }

        // Got everything we need
        if meta.slug.is_some() && meta.version.is_some() && meta.cwd.is_some() {
            break;
        }
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn encode_project_path_basic() {
        let path = PathBuf::from("/home/atobey/src/kaijutsu");
        assert_eq!(encode_project_path(&path), "-home-atobey-src-kaijutsu");
    }

    #[test]
    fn encode_project_path_root() {
        let path = PathBuf::from("/");
        assert_eq!(encode_project_path(&path), "-");
    }

    #[test]
    fn encode_project_path_single_component() {
        let path = PathBuf::from("/tmp");
        assert_eq!(encode_project_path(&path), "-tmp");
    }

    #[test]
    fn parse_session_metadata_from_jsonl() {
        let dir = std::env::temp_dir().join("kj-agent-test");
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl_path = dir.join("test-session.jsonl");

        let line = serde_json::json!({
            "type": "user",
            "sessionId": "abc-123",
            "slug": "testing-slug",
            "version": "2.1.45",
            "cwd": "/home/test/project",
            "message": {"role": "user", "content": "hello"}
        });
        std::fs::write(&jsonl_path, format!("{}\n", line)).unwrap();

        let meta = parse_session_metadata(&jsonl_path);
        assert_eq!(meta.slug.as_deref(), Some("testing-slug"));
        assert_eq!(meta.version.as_deref(), Some("2.1.45"));
        assert_eq!(meta.cwd.as_deref(), Some("/home/test/project"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn most_recent_jsonl_picks_newest() {
        let dir = std::env::temp_dir().join("kj-agent-mru-test");
        std::fs::create_dir_all(&dir).unwrap();

        // Create two files with different mtimes
        let old = dir.join("old-session.jsonl");
        let new = dir.join("new-session.jsonl");
        std::fs::write(&old, "{}").unwrap();
        // Small sleep to ensure different mtime
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&new, "{}").unwrap();

        let result = most_recent_jsonl(&dir).unwrap();
        assert_eq!(result.file_name().unwrap(), "new-session.jsonl");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn most_recent_jsonl_ignores_non_jsonl() {
        let dir = std::env::temp_dir().join("kj-agent-filter-test");
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("not-a-session.txt"), "{}").unwrap();
        std::fs::write(dir.join("session.jsonl"), "{}").unwrap();

        let result = most_recent_jsonl(&dir).unwrap();
        assert_eq!(result.file_name().unwrap(), "session.jsonl");

        std::fs::remove_dir_all(&dir).ok();
    }
}
