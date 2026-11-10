//! The terminal-fit panic probes and the rule that decides whether a panic
//! unwinds the event loop.

use parking_lot::Mutex;

/// `KAIJUTSU_TUI_PROBE_PANIC`: unset, or `frame` to panic inside the frame
/// — where a synchronized update is open and the restore path has to end it
/// — or `task` to panic inside a `spawn_local` task the loop never joins,
/// the way `Feeds::pump` or a hydrate round would — or any other value to
/// panic on the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbePanic {
    Off,
    OnKey,
    InFrame,
    InTask,
}

impl ProbePanic {
    pub(super) fn from_env() -> Self {
        match std::env::var("KAIJUTSU_TUI_PROBE_PANIC").ok().as_deref() {
            None => Self::Off,
            Some("frame") => Self::InFrame,
            Some("task") => Self::InTask,
            Some(_) => Self::OnKey,
        }
    }
}

/// The event loop's own thread, recorded once when the panic hook is
/// installed (`run`). Only a panic on this thread that is also outside any
/// tokio task unwinds `LocalSet::run_until`'s root future — the hook's own
/// [`panic_unwinds_the_loop`] check.
pub(super) static LOOP_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// A panic the hook saw but did not unwind the loop for — inside a
/// `spawn_local` task nobody joins (`Feeds::pump`, a hydrate round), or on a
/// background thread (the key reader, the peer thread). Recorded here so
/// `event_loop` can notice it and end the loop with an `Err` instead of
/// leaving a panicked task's silence to draw over a screen the hook left
/// alone. `event_loop` takes it, so a second unnoticed panic before the
/// first is drained overwrites rather than queues — the loop is about to end
/// either way.
pub(super) static TASK_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Whether a panic on `panicking_thread` unwinds `run_until`'s root future —
/// the only case [`super::restore_terminal`] may run from inside the hook.
/// `loop_thread` is [`LOOP_THREAD`] as the hook read it; `task_id` is
/// `tokio::task::try_id()` on the panicking thread.
///
/// A `spawn_local` task shares the loop's own OS thread (`main.rs` drives
/// every task through one `LocalSet::run_until` on the runtime's main
/// thread) but panics without reaching the root future itself —
/// `tokio::task::try_id()` is `Some` inside one and `None` at the root — so
/// a thread-id check alone cannot tell a task panic from the root's own. A
/// panic on any other thread does not reach the root future either.
pub(super) fn panic_unwinds_the_loop(
    panicking_thread: std::thread::ThreadId,
    loop_thread: Option<std::thread::ThreadId>,
    task_id: Option<tokio::task::Id>,
) -> bool {
    Some(panicking_thread) == loop_thread && task_id.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ────────────────────────────────────────────────────────────────────
    // The panic hook (docs/tui.md, "Every way out restores the terminal")
    // ────────────────────────────────────────────────────────────────────

    /// The design fact the hook's restore-or-not decision rests on:
    /// `tokio::task::try_id()` is `None` for `LocalSet::run_until`'s own
    /// root future and `Some` inside a task `spawn_local` puts on that same
    /// `LocalSet` — even though both run on the very thread that drives the
    /// `LocalSet`, so a thread id alone cannot tell them apart.
    #[test]
    fn try_id_is_some_inside_a_spawned_task_and_none_at_the_root() {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("current-thread runtime");
        let local = tokio::task::LocalSet::new();
        let (root_id, task_id) = runtime.block_on(local.run_until(async {
            let root_id = tokio::task::try_id();
            let task_id = tokio::task::spawn_local(async { tokio::task::try_id() })
                .await
                .expect("the spawned task did not panic");
            (root_id, task_id)
        }));
        assert_eq!(root_id, None, "the run_until root future is not itself inside a task");
        assert!(task_id.is_some(), "a spawn_local task carries a task id");
    }

    /// [`panic_unwinds_the_loop`] says yes only for the root panic on the
    /// loop's own thread — a task sharing that thread, and a panic on any
    /// other thread, both say no (`docs/issues.md` before this fix named a
    /// thread-id check alone, which the test above falsifies).
    #[test]
    fn panic_unwinds_the_loop_only_at_the_root_on_the_loop_thread() {
        let loop_thread = std::thread::current().id();
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("current-thread runtime");
        let handle = runtime.spawn(async {});
        let task_id = handle.id();
        runtime.block_on(handle).expect("the spawned task did not panic");
        let other_thread =
            std::thread::spawn(|| std::thread::current().id()).join().expect("the probe thread joins");

        assert!(
            panic_unwinds_the_loop(loop_thread, Some(loop_thread), None),
            "the root panic, on the loop thread and in no task, unwinds it"
        );
        assert!(
            !panic_unwinds_the_loop(loop_thread, Some(loop_thread), Some(task_id)),
            "a task panic, even on the loop thread, does not unwind it"
        );
        assert!(
            !panic_unwinds_the_loop(other_thread, Some(loop_thread), None),
            "a panic on another thread never unwinds the loop"
        );
        assert!(
            !panic_unwinds_the_loop(loop_thread, None, None),
            "no recorded loop thread means the hook cannot tell, so it must not restore"
        );
    }
}
