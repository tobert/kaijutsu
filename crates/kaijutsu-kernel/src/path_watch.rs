//! The kernel's one host file watcher for `kj watch` (`docs/watch.md`).
//!
//! It watches the parent directory of each watched file, so an editor that
//! saves by writing a new file and renaming it over the old one is still
//! seen. Events are recorded into `context_watches` on notify's own thread;
//! nothing here starts a turn or runs on the beat path.
//!
//! The database decides who an event reaches: only live contexts' watches
//! accrue. The watched set is only the cost of listening, so a watch removed
//! or archived without a [`PathWatcher::sync`] still records nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{EventKind, RecursiveMode, Watcher};
use parking_lot::Mutex;

use crate::kernel_db::KernelDb;

/// Events on one file closer together than this are one change.
pub(crate) const BURST_MS: i64 = 500;

#[derive(Default)]
struct Files {
    /// Host files some live context watches.
    watched: HashSet<PathBuf>,
    /// The latest event per file, for burst detection.
    last_event: HashMap<PathBuf, i64>,
}

#[derive(Default)]
pub struct PathWatcher {
    files: Arc<Mutex<Files>>,
    listener: Mutex<Listener>,
}

#[derive(Default)]
struct Listener {
    watcher: Option<notify::RecommendedWatcher>,
    dirs: HashSet<PathBuf>,
}

impl PathWatcher {
    /// Listen to exactly the files live contexts watch now. Call after a
    /// watch is added and at kernel start.
    pub fn sync(&self, db: &Arc<Mutex<KernelDb>>) -> Result<(), String> {
        let paths = db.lock().watched_host_paths().map_err(|e| e.to_string())?;
        let watched: HashSet<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        let dirs: HashSet<PathBuf> = watched.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
        let mut listener = self.listener.lock();
        if listener.watcher.is_none() && !dirs.is_empty() {
            let files = self.files.clone();
            let db = Arc::downgrade(db);
            let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let event = match event {
                    Ok(event) => event,
                    Err(e) => {
                        tracing::error!(error = %e, "kj watch: the host file watcher reported an error");
                        return;
                    }
                };
                if !counts(&event.kind) {
                    return;
                }
                let Some(db) = db.upgrade() else { return };
                let at = now_millis();
                for path in &event.paths {
                    record(&files, &db, path, path.exists(), at);
                }
            }).map_err(|e| format!("start the host file watcher: {e}"))?;
            listener.watcher = Some(watcher);
        }
        let Listener { watcher, dirs: held } = &mut *listener;
        if let Some(watcher) = watcher.as_mut() {
            for dir in held.difference(&dirs) {
                if let Err(e) = watcher.unwatch(dir) {
                    tracing::warn!(dir = %dir.display(), error = %e, "kj watch: unwatch failed");
                }
            }
            for dir in dirs.difference(held) {
                watcher.watch(dir, RecursiveMode::NonRecursive).map_err(|e| format!("watch {}: {e}", dir.display()))?;
            }
        }
        *held = dirs;
        let mut files = self.files.lock();
        files.last_event.retain(|path, _| watched.contains(path));
        files.watched = watched;
        Ok(())
    }

    /// Record one event on `path` at `at`, as the listener does. The seam
    /// tests drive instead of the host file system.
    pub fn record_event(&self, db: &Arc<Mutex<KernelDb>>, path: &Path, present: bool, at: i64) {
        record(&self.files, db, path, present, at);
    }
}

/// Content and presence changes count; reads and metadata do not. Closing a
/// written file ends its write, so it joins the write's burst.
fn counts(kind: &EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) | EventKind::Any | EventKind::Other => false,
    }
}

fn record(files: &Mutex<Files>, db: &Mutex<KernelDb>, path: &Path, present: bool, at: i64) {
    let new_burst = {
        let mut files = files.lock();
        if !files.watched.contains(path) {
            return;
        }
        let new_burst = files.last_event.get(path).is_none_or(|last| at - last >= BURST_MS);
        files.last_event.insert(path.to_path_buf(), at);
        new_burst
    };
    if let Err(e) = db.lock().record_path_event(&path.to_string_lossy(), present, at, new_burst) {
        tracing::error!(path = %path.display(), error = %e, "kj watch: recording a change failed");
    }
}

/// The notice a prompt carries for `pending`: one line per watch naming the
/// path, its changes, when it last changed, and whether it is gone. It holds
/// no file content.
pub(crate) fn render_notice(pending: &[crate::kernel_db::WatchRow]) -> String {
    let mut lines = vec!["Watched files changed since the last prompt:".to_string()];
    for w in pending {
        let times = if w.changes == 1 { "once".to_string() } else { format!("{} times", w.changes) };
        let last = w.last_change_at.map(crate::kj::format::format_timestamp).unwrap_or_else(|| "at an unknown time".into());
        let gone = if w.present == Some(false) { "; it no longer exists" } else { "" };
        lines.push(format!("- {}: changed {times}, last {last}{gone}", w.target));
    }
    lines.join("\n")
}

pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;
    use std::path::Path;

    async fn watched(dir: &Path) -> (crate::kj::KjDispatcher, kaijutsu_types::ContextId, std::path::PathBuf) {
        std::fs::write(dir.join("AGENTS.md"), "rules").unwrap();
        let d = test_dispatcher().await;
        d.kernel().mount("/mnt/w", crate::vfs::LocalBackend::new(dir)).await;
        let ctx = register_context(&d, Some("w"), None, PrincipalId::new());
        let added = d.dispatch(&["watch".into(), "add".into(), "/mnt/w/AGENTS.md".into()], &caller_with_context(ctx)).await;
        assert!(added.is_ok(), "{added:?}");
        let host = dunce::canonicalize(dir.join("AGENTS.md")).unwrap();
        (d, ctx, host)
    }

    /// Events closer than the burst window are one change, and a file no
    /// live context watches records nothing. Falsified by counting raw events.
    #[tokio::test]
    async fn events_coalesce_into_bursts() {
        let dir = tempfile::tempdir().unwrap();
        let (d, ctx, host) = watched(dir.path()).await;
        let k = d.kernel();
        for at in [1_000, 1_100, 1_200, 1_200 + super::BURST_MS + 1, 1_800] {
            k.path_watcher().record_event(k.kernel_db(), &host, true, at);
        }
        k.path_watcher().record_event(k.kernel_db(), &dir.path().join("other"), true, 5_000);
        let pending = k.kernel_db().lock().pending_watches(ctx).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].changes, pending[0].first_change_at, pending[0].last_change_at), (2, Some(1_000), Some(1_800)));
    }

    /// The host listener sees a plain write and a save that renames a new
    /// file over the old one, and starts no turn.
    #[tokio::test]
    async fn the_listener_sees_writes_and_rename_saves() {
        let dir = tempfile::tempdir().unwrap();
        let (d, ctx, host) = watched(dir.path()).await;
        let k = d.kernel();
        let changes = || k.kernel_db().lock().list_watches(ctx).unwrap()[0].changes;
        let wait_for = |n: u64| async move {
            for _ in 0..100 {
                if changes() >= n { return; }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("the watch reached {} changes, not {n}", changes());
        };
        std::fs::write(&host, "rules, edited").unwrap();
        wait_for(1).await;
        tokio::time::sleep(std::time::Duration::from_millis(super::BURST_MS as u64 + 100)).await;
        let tmp = dir.path().join(".AGENTS.md.swp");
        std::fs::write(&tmp, "rules, saved by rename").unwrap();
        std::fs::rename(&tmp, &host).unwrap();
        wait_for(2).await;
        assert!(!k.turn_in_flight(ctx), "a watch never starts a turn");
    }
}
