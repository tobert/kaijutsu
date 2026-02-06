//! Git operations via libgit2.
//!
//! Focused subset of git operations for the GitEngine.
//! Adapted from kaish's `GitVfs` without filesystem trait coupling.

use git2::{
    Commit, DiffOptions, IndexAddOption, Oid, Repository, Signature, Status, StatusOptions,
    StatusShow,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Thin wrapper around a git2 `Repository`.
pub struct GitRepo {
    repo: Mutex<Repository>,
    root: PathBuf,
}

/// Errors from git operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git2: {0}")]
    Git2(#[from] git2::Error),
    #[error("{0}")]
    Other(String),
}

/// Status of a single file in the working tree.
#[derive(Debug, Clone)]
pub struct FileStatus {
    /// Path relative to repository root.
    pub path: String,
    /// Git status flags.
    pub status: Status,
}

impl FileStatus {
    /// Porcelain-style two-character status code.
    pub fn status_chars(&self) -> &'static str {
        if self.status.is_index_new() {
            "A "
        } else if self.status.is_index_modified() {
            "M "
        } else if self.status.is_index_deleted() {
            "D "
        } else if self.status.is_wt_modified() {
            " M"
        } else if self.status.is_wt_new() {
            "??"
        } else if self.status.is_wt_deleted() {
            " D"
        } else if self.status.is_wt_renamed() {
            " R"
        } else {
            "  "
        }
    }
}

/// Summary of repository status.
#[derive(Debug, Clone, Default)]
pub struct StatusSummary {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

/// A single log entry (commit).
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Full commit OID.
    pub oid: String,
    /// Short (7-char) commit ID.
    pub short_id: String,
    /// Commit message.
    pub message: String,
    /// Author name.
    pub author: String,
    /// Author email.
    pub email: String,
    /// Commit timestamp (Unix seconds).
    pub time: i64,
}

impl GitRepo {
    /// Open an existing git repository at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, GitError> {
        let root: PathBuf = path.into();
        let repo = Repository::open(&root)?;
        Ok(Self {
            repo: Mutex::new(repo),
            root,
        })
    }

    /// Repository root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ========================================================================
    // Status
    // ========================================================================

    /// Get the current status of the working tree.
    pub fn status(&self) -> Result<Vec<FileStatus>, GitError> {
        let repo = self.lock()?;

        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .show(StatusShow::IndexAndWorkdir);

        let statuses = repo.statuses(Some(&mut opts))?;
        let mut result = Vec::with_capacity(statuses.len());

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                result.push(FileStatus {
                    path: path.to_string(),
                    status: entry.status(),
                });
            }
        }

        Ok(result)
    }

    /// Get a simplified status summary.
    pub fn status_summary(&self) -> Result<StatusSummary, GitError> {
        let statuses = self.status()?;
        let mut summary = StatusSummary::default();

        for file in &statuses {
            if file.status.is_index_new() || file.status.is_index_modified() {
                summary.staged += 1;
            }
            if file.status.is_wt_modified() || file.status.is_wt_new() {
                summary.modified += 1;
            }
            if file.status.is_wt_new() && !file.status.is_index_new() {
                summary.untracked += 1;
            }
        }

        Ok(summary)
    }

    // ========================================================================
    // Diff
    // ========================================================================

    /// Diff between working tree and HEAD (staged + unstaged).
    pub fn diff(&self) -> Result<String, GitError> {
        let repo = self.lock()?;

        let head = repo.head()?;
        let head_tree = head.peel_to_tree()?;

        let mut opts = DiffOptions::new();
        opts.include_untracked(true);

        let diff =
            repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))?;

        diff_to_string(&diff)
    }

    /// Diff of staged changes only (index vs HEAD).
    pub fn diff_cached(&self) -> Result<String, GitError> {
        let repo = self.lock()?;

        let head_tree = match repo.head() {
            Ok(head) => Some(head.peel_to_tree()?),
            Err(_) => None, // first commit — diff against empty tree
        };

        let diff = repo.diff_tree_to_index(
            head_tree.as_ref(),
            Some(&repo.index()?),
            None,
        )?;

        diff_to_string(&diff)
    }

    // ========================================================================
    // Index
    // ========================================================================

    /// Add files to the index (staging area) by pathspec.
    pub fn add(&self, pathspec: &[&str]) -> Result<(), GitError> {
        let repo = self.lock()?;
        let mut index = repo.index()?;

        let specs: Vec<String> = pathspec.iter().map(|s| s.to_string()).collect();
        index.add_all(
            specs.iter().map(|s| s.as_str()),
            IndexAddOption::DEFAULT,
            None,
        )?;

        index.write()?;
        Ok(())
    }

    // ========================================================================
    // Commit
    // ========================================================================

    /// Create a commit with the currently staged changes.
    pub fn commit(&self, message: &str, author: Option<&str>) -> Result<Oid, GitError> {
        if message.is_empty() {
            return Err(GitError::Other("commit message cannot be empty".into()));
        }

        let repo = self.lock()?;
        let mut index = repo.index()?;
        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;

        let sig = if let Some(author_str) = author {
            if let Some((name, email)) = parse_author(author_str) {
                Signature::now(&name, &email)?
            } else {
                repo.signature()?
            }
        } else {
            repo.signature()?
        };

        let parent = match repo.head() {
            Ok(head) => Some(head.peel_to_commit()?),
            Err(_) => None,
        };

        let parents: Vec<&Commit> = parent.iter().collect();
        let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;

        Ok(oid)
    }

    // ========================================================================
    // Log
    // ========================================================================

    /// Get recent commit log entries.
    pub fn log(&self, count: usize) -> Result<Vec<LogEntry>, GitError> {
        let repo = self.lock()?;

        if repo.head().is_err() {
            return Ok(Vec::new());
        }

        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;

        let mut entries = Vec::with_capacity(count);

        for (i, oid) in revwalk.enumerate() {
            if i >= count {
                break;
            }

            let oid = oid?;
            let commit = repo.find_commit(oid)?;

            entries.push(LogEntry {
                oid: oid.to_string(),
                short_id: oid.to_string()[..7].to_string(),
                message: commit.message().unwrap_or("").to_string(),
                author: commit.author().name().unwrap_or("").to_string(),
                email: commit.author().email().unwrap_or("").to_string(),
                time: commit.time().seconds(),
            });
        }

        Ok(entries)
    }

    // ========================================================================
    // Branch
    // ========================================================================

    /// Get the current branch name (None if detached HEAD or no commits).
    pub fn current_branch(&self) -> Result<Option<String>, GitError> {
        let repo = self.lock()?;

        match repo.head() {
            Ok(head) => {
                if head.is_branch() {
                    Ok(head.shorthand().map(|s| s.to_string()))
                } else {
                    Ok(None)
                }
            }
            Err(_) => Ok(None),
        }
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Repository>, GitError> {
        self.repo
            .lock()
            .map_err(|_| GitError::Other("failed to acquire repository lock".into()))
    }
}

/// Format a git2 Diff as a patch string.
fn diff_to_string(diff: &git2::Diff<'_>) -> Result<String, GitError> {
    let mut output = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = match line.origin() {
            '+' => "+",
            '-' => "-",
            ' ' => " ",
            'H' | 'F' | 'B' => "",
            _ => "",
        };
        if !origin.is_empty() {
            output.push_str(origin);
        }
        if let Ok(content) = std::str::from_utf8(line.content()) {
            output.push_str(content);
        }
        true
    })?;
    Ok(output)
}

/// Parse "Name <email>" format.
fn parse_author(s: &str) -> Option<(String, String)> {
    let lt_pos = s.find('<')?;
    let gt_pos = s.find('>')?;
    let name = s[..lt_pos].trim().to_string();
    let email = s[lt_pos + 1..gt_pos].trim().to_string();
    Some((name, email))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_repo() -> (GitRepo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "Test User").unwrap();
            config.set_str("user.email", "test@example.com").unwrap();
        }
        drop(repo);
        let git_repo = GitRepo::open(dir.path()).unwrap();
        (git_repo, dir)
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn test_open_and_root() {
        let (repo, dir) = setup_repo();
        assert_eq!(repo.root(), dir.path());
    }

    #[test]
    fn test_status_clean() {
        let (repo, _dir) = setup_repo();
        let status = repo.status().unwrap();
        assert!(status.is_empty());
    }

    #[test]
    fn test_status_dirty() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "hello.txt", "hello");
        let status = repo.status().unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, "hello.txt");
        assert!(status[0].status.is_wt_new());
    }

    #[test]
    fn test_add_and_commit() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "hello.txt", "hello");
        repo.add(&["hello.txt"]).unwrap();

        let oid = repo.commit("Initial commit", None).unwrap();
        assert!(!oid.is_zero());

        let status = repo.status().unwrap();
        assert!(status.is_empty());
    }

    #[test]
    fn test_commit_empty_message_fails() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "hello.txt", "hello");
        repo.add(&["hello.txt"]).unwrap();

        let result = repo.commit("", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_diff() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "hello.txt", "hello");
        repo.add(&["hello.txt"]).unwrap();
        repo.commit("init", None).unwrap();

        write_file(dir.path(), "hello.txt", "hello world");
        let diff = repo.diff().unwrap();
        assert!(diff.contains("+hello world"));
        assert!(diff.contains("-hello"));
    }

    #[test]
    fn test_diff_cached() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "hello.txt", "hello");
        repo.add(&["hello.txt"]).unwrap();
        repo.commit("init", None).unwrap();

        // Modify and stage
        write_file(dir.path(), "hello.txt", "hello world");
        repo.add(&["hello.txt"]).unwrap();

        let cached = repo.diff_cached().unwrap();
        assert!(cached.contains("+hello world"));

        // Unstaged change should NOT appear in diff_cached
        write_file(dir.path(), "hello.txt", "hello world extra");
        let cached2 = repo.diff_cached().unwrap();
        assert!(!cached2.contains("extra"));
    }

    #[test]
    fn test_diff_cached_empty_when_nothing_staged() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "hello.txt", "hello");
        repo.add(&["hello.txt"]).unwrap();
        repo.commit("init", None).unwrap();

        let cached = repo.diff_cached().unwrap();
        assert!(cached.is_empty());
    }

    #[test]
    fn test_log() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "a.txt", "a");
        repo.add(&["a.txt"]).unwrap();
        repo.commit("first", None).unwrap();

        write_file(dir.path(), "b.txt", "b");
        repo.add(&["b.txt"]).unwrap();
        repo.commit("second", None).unwrap();

        let log = repo.log(10).unwrap();
        assert_eq!(log.len(), 2);
        assert!(log[0].message.contains("second"));
        assert!(log[1].message.contains("first"));
        assert_eq!(log[0].author, "Test User");
    }

    #[test]
    fn test_log_empty_repo() {
        let (repo, _dir) = setup_repo();
        let log = repo.log(10).unwrap();
        assert!(log.is_empty());
    }

    #[test]
    fn test_current_branch() {
        let (repo, dir) = setup_repo();
        // No commits = no branch
        assert_eq!(repo.current_branch().unwrap(), None);

        write_file(dir.path(), "a.txt", "a");
        repo.add(&["a.txt"]).unwrap();
        repo.commit("init", None).unwrap();

        let branch = repo.current_branch().unwrap();
        assert!(branch.is_some());
    }

    #[test]
    fn test_status_summary() {
        let (repo, dir) = setup_repo();
        write_file(dir.path(), "staged.txt", "staged");
        repo.add(&["staged.txt"]).unwrap();

        write_file(dir.path(), "untracked.txt", "untracked");

        let summary = repo.status_summary().unwrap();
        assert_eq!(summary.staged, 1);
        assert!(summary.untracked >= 1);
    }

    #[test]
    fn test_parse_author() {
        assert_eq!(
            parse_author("Amy Tobey <amy@example.com>"),
            Some(("Amy Tobey".to_string(), "amy@example.com".to_string()))
        );
        assert_eq!(parse_author("invalid"), None);
    }
}
