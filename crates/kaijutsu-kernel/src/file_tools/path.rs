//! Canonical path resolution shared by the MCP file engines and the kaish
//! `MountBackend`.
//!
//! Why this exists: `FileDocumentCache` keys every CRDT document by
//! `file_context_id(path)` — a UUIDv5 of the path *string* (see `cache.rs`).
//! If two callers address the same real file with different strings
//! (`foo.rs`, `./foo.rs`, `/abs/foo.rs`), they get three different CRDT
//! documents and the surfaces silently diverge. Both the MCP file tools and
//! the kaish file builtins must therefore canonicalize to one absolute path
//! *before* the cache sees it.
//!
//! Resolution is lexical only — no filesystem access, no symlink resolution.
//! `.` is dropped and `..` pops the previous component. A `..` that would
//! traverse above the root is an **error**, not silently clamped: this input
//! comes from models and agents, so we crash rather than guess (and never
//! corrupt by escaping the namespace). This mirrors kaish-kernel's
//! `ExecContext::resolve_path`/`normalize_path` but is stricter about escape.

use std::path::{Component, Path, PathBuf};

/// Failure modes for [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// A `..` component tried to traverse above the filesystem root.
    EscapesRoot(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::EscapesRoot(p) => {
                write!(f, "path {:?} escapes above the filesystem root", p)
            }
        }
    }
}

impl std::error::Error for PathError {}

/// Resolve `path` against `cwd` into a canonical absolute path.
///
/// - Absolute `path` (leading `/`) ignores `cwd`.
/// - Relative `path` is joined onto `cwd` (which is expected to be absolute;
///   it defaults to `/` everywhere a context lacks one).
/// - `.` and `..` are normalized lexically; `..` above root errors.
pub fn resolve(cwd: &Path, path: &str) -> Result<PathBuf, PathError> {
    let raw = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    normalize(&raw, path)
}

/// Resolve and return the canonical path as a `String` for use as a cache key.
///
/// Convenience for the common case of feeding `FileDocumentCache`, whose APIs
/// take `&str`. The string form of a normalized absolute path is stable on a
/// given platform, so two callers resolving the same file produce byte-identical
/// keys (and thus the same `file_context_id`).
pub fn resolve_str(cwd: &Path, path: &str) -> Result<String, PathError> {
    Ok(resolve(cwd, path)?.to_string_lossy().into_owned())
}

/// True if the (already-canonicalized) path is under the rc tree
/// (`/etc/rc` or `/etc/rc/...`). Writing here via the file tools is gated on
/// the `rc-write` capability; everything else under `/etc` is denied flat.
/// Delegates to the canonical, component-boundary-correct predicate in
/// `kaijutsu_types::paths` — the single source of truth for this check,
/// shared with `editor::config_owned` and the SFTP gate.
pub(crate) use kaijutsu_types::paths::is_rc_path;

/// The denial returned when a context without `rc-write` tries to write an
/// rc script via the file tools. Names the deliberate paths so the nudge is
/// actionable, not a dead end.
pub(crate) fn rc_write_denied(path: &str) -> crate::execution::ExecResult {
    crate::execution::ExecResult::failure(
        1,
        format!(
            "file write to '{path}' needs the rc-write capability \
             (grant it with `kj binding allow rc-write`, or edit via `kj rc` / host editor)"
        ),
    )
}

/// Flat deny for writes under `/etc` *outside* the rc tree via the
/// `file:write`/`edit` tools. The rc tree (`/etc/rc`) is handled separately
/// with the `rc-write` capability; everything else under `/etc` maps to the
/// host's read-only root mount and is not a kaijutsu write surface. Returns
/// `Some(failure)` if the path is under `/etc`, else `None`.
pub(crate) fn deny_etc_write(canonical_path: &str) -> Option<crate::execution::ExecResult> {
    if canonical_path == "/etc" || canonical_path.starts_with("/etc/") {
        return Some(crate::execution::ExecResult::failure(
            1,
            format!("file write denied under /etc: '{canonical_path}'"),
        ));
    }
    None
}

fn normalize(path: &Path, original: &str) -> Result<PathBuf, PathError> {
    let mut parts: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {} // drop `.`
            Component::ParentDir => match parts.last() {
                // Pop a real directory component...
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                // ...but `..` at/above root is an escape attempt, not a no-op.
                _ => return Err(PathError::EscapesRoot(original.to_string())),
            },
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        Ok(PathBuf::from("/"))
    } else {
        Ok(parts.iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn is_rc_path_identifies_the_rc_tree() {
        assert!(is_rc_path("/etc/rc"));
        assert!(is_rc_path("/etc/rc/coder/create/S00-stance.md"));
        // Not the rc tree:
        assert!(!is_rc_path("/etc/passwd"));
        assert!(!is_rc_path("/etc"));
        assert!(!is_rc_path("/etcrc")); // no slash boundary
        assert!(!is_rc_path("/src/kaijutsu/foo.rs"));
    }

    #[test]
    fn deny_etc_write_blocks_etc_but_passes_others() {
        // The rc tree is handled by is_rc_path + rc-write capability, not here;
        // deny_etc_write is the flat deny for the rest of /etc (host root).
        assert!(deny_etc_write("/etc").is_some());
        assert!(deny_etc_write("/etc/passwd").is_some());
        // Everything outside /etc is allowed through (workspace guard applies).
        assert!(deny_etc_write("/src/kaijutsu/foo.rs").is_none());
        assert!(deny_etc_write("/tmp/scratch").is_none());
        // Not fooled by a prefix that merely starts with the letters "etc".
        assert!(deny_etc_write("/etcetera/x").is_none());
    }

    #[test]
    fn relative_joins_cwd() {
        assert_eq!(
            resolve(&cwd("/src/kaijutsu"), "foo.rs").unwrap(),
            PathBuf::from("/src/kaijutsu/foo.rs")
        );
    }

    #[test]
    fn dot_slash_is_equivalent_to_bare() {
        assert_eq!(
            resolve(&cwd("/src/kaijutsu"), "./foo.rs").unwrap(),
            resolve(&cwd("/src/kaijutsu"), "foo.rs").unwrap()
        );
    }

    #[test]
    fn absolute_ignores_cwd() {
        assert_eq!(
            resolve(&cwd("/somewhere/else"), "/abs/foo.rs").unwrap(),
            PathBuf::from("/abs/foo.rs")
        );
    }

    #[test]
    fn parent_dir_normalizes() {
        assert_eq!(
            resolve(&cwd("/src/kaijutsu/crates"), "../foo.rs").unwrap(),
            PathBuf::from("/src/kaijutsu/foo.rs")
        );
        assert_eq!(
            resolve(&cwd("/a/b/c"), "../../x").unwrap(),
            PathBuf::from("/a/x")
        );
    }

    #[test]
    fn embedded_dot_dot_normalizes() {
        assert_eq!(
            resolve(&cwd("/"), "/a/b/../c").unwrap(),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn escape_above_root_is_an_error() {
        assert_eq!(
            resolve(&cwd("/"), "../etc/passwd"),
            Err(PathError::EscapesRoot("../etc/passwd".into()))
        );
        assert!(resolve(&cwd("/a"), "/a/../..").is_err());
        assert!(resolve(&cwd("/a/b"), "../../../../escape").is_err());
    }

    #[test]
    fn root_resolves_to_root() {
        assert_eq!(resolve(&cwd("/x"), "/").unwrap(), PathBuf::from("/"));
    }

    /// The whole point: different spellings of one file produce one key.
    #[test]
    fn equivalent_spellings_canonicalize_identically() {
        let target = "/src/kaijutsu/foo.rs";
        let a = resolve_str(&cwd("/src/kaijutsu"), "foo.rs").unwrap();
        let b = resolve_str(&cwd("/src/kaijutsu"), "./foo.rs").unwrap();
        let c = resolve_str(&cwd("/src/kaijutsu/crates"), "../foo.rs").unwrap();
        let d = resolve_str(&cwd("/anywhere"), target).unwrap();
        assert_eq!(a, target);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a, d);
    }
}
