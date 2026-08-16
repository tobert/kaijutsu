//! The PID-reuse guard.
//!
//! A `<pid>.json` descriptor can outlive the process that wrote it (a crash
//! that skipped cleanup, a killed session) and the OS is free to hand that
//! pid to an unrelated process later. Presenting a recycled pid as "this is
//! still that Claude Code session" would be actively wrong — worse than
//! saying nothing. Claude Code defends against exactly this by recording
//! `procStart`: field 22 (`starttime`) of `/proc/<pid>/stat` at the moment
//! the session started, measured in clock ticks since boot **[probed:
//! cross-checked three ways against a live session]**. That field is
//! monotonic per-boot and reused pids get a new (larger) starttime, so
//! comparing the recorded value against the live process's current starttime
//! tells alive-and-same-process apart from alive-but-a-stranger.
//!
//! CC's own code ranks candidates by this and reports `dead-owner`
//! **[source]**.

use std::path::Path;

/// How a descriptor's recorded pid maps onto the live process table right
/// now. Never collapsed into a bare bool — "stale" (pid reused by a stranger)
/// and "gone" (nothing there at all) are different facts a caller might act
/// on differently, and `Alive` is the only one that means "this is safe to
/// treat as the same session that wrote the descriptor."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachable {
    /// The pid is running and its live `/proc` starttime matches the
    /// descriptor's recorded `procStart` — genuinely the same process.
    Alive,
    /// The pid is running, but its starttime disagrees — this is a different
    /// process that reused the pid after the original session exited.
    Stale,
    /// No process with this pid exists at all.
    Gone,
    /// We could not determine liveness: `/proc/<pid>/stat` was unreadable for
    /// a reason other than the process being absent, its format was not one we
    /// recognize, or the descriptor's own `procStart` was not a number.
    ///
    /// This variant exists so those cases cannot masquerade as `Gone`. `Gone`
    /// asserts a fact ("that process is dead"); if a `/proc` format shift or
    /// a bug in our parser were folded into it, *every* session would report
    /// dead and a roster would look plausibly empty instead of obviously
    /// broken — a silent failure that reads as a true answer.
    Unknown,
}

impl Reachable {
    pub fn label(self) -> &'static str {
        match self {
            Reachable::Alive => "alive",
            Reachable::Stale => "stale",
            Reachable::Gone => "gone",
            Reachable::Unknown => "unknown",
        }
    }
}

/// Outcome of reading field 22 (`starttime`) from a `stat` file.
///
/// The three cases are kept apart on purpose: only an absent process licenses
/// [`Reachable::Gone`]. See that variant's note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcProbe {
    Started(u64),
    NoSuchProcess,
    Undetermined(String),
}

/// Read `/proc/<pid>/stat` under `proc_root` (normally `/proc`) and extract
/// the starttime. Kept root-injectable so tests can point it at a fixture
/// tree instead of the live process table.
pub fn probe_proc_starttime_in(proc_root: &Path, pid: u32) -> ProcProbe {
    let path = proc_root.join(pid.to_string()).join("stat");
    match std::fs::read_to_string(&path) {
        Ok(raw) => match parse_stat_starttime(&raw) {
            Some(s) => ProcProbe::Started(s),
            None => ProcProbe::Undetermined(format!("{}: unrecognized format", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProcProbe::NoSuchProcess,
        Err(e) => ProcProbe::Undetermined(format!("{}: {e}", path.display())),
    }
}

/// [`probe_proc_starttime_in`] against the live `/proc`.
pub fn probe_proc_starttime(pid: u32) -> ProcProbe {
    probe_proc_starttime_in(Path::new("/proc"), pid)
}

/// Extract field 22 (`starttime`) from a raw `/proc/<pid>/stat` line.
///
/// Field 2 (`comm`, the process name) is wrapped in parentheses and MAY
/// itself contain spaces or parentheses (e.g. a process renamed to `weird
/// (name) here`), so splitting the whole line on whitespace misindexes every
/// field that follows. The kernel's own `/proc` formatter guarantees comm
/// cannot contain a `)` that is followed by " (" — so the LAST `)` in the
/// line unambiguously ends the comm field; everything after it is field 3
/// (`state`) onward, safe to whitespace-split. Field 22 is then at index 19
/// of that split (state=index 0, ..., starttime=field 22=index 22-3=19).
pub fn parse_stat_starttime(stat_line: &str) -> Option<u64> {
    let close = stat_line.rfind(')')?;
    let rest = stat_line.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(19)?.parse::<u64>().ok()
}

/// Compare a descriptor's recorded `procStart` against the live process's
/// current starttime, read under `proc_root`.
///
/// Returns the classification plus a reason whenever the answer is
/// [`Reachable::Unknown`]. The reason is not decoration: an undetermined
/// liveness is reported alongside parse failures so it cannot pass as a quiet
/// label in a table nobody reads closely.
pub fn classify_reachability_in(
    proc_root: &Path,
    pid: u32,
    recorded_proc_start: &str,
) -> (Reachable, Option<String>) {
    let live_start = match probe_proc_starttime_in(proc_root, pid) {
        ProcProbe::Started(s) => s,
        ProcProbe::NoSuchProcess => return (Reachable::Gone, None),
        ProcProbe::Undetermined(why) => return (Reachable::Unknown, Some(why)),
    };
    match recorded_proc_start.trim().parse::<u64>() {
        Ok(recorded) if recorded == live_start => (Reachable::Alive, None),
        Ok(_) => (Reachable::Stale, None),
        // A non-numeric `procStart` is a corrupt descriptor, not a reused pid.
        // Calling it `Stale` would assert a specific thing we did not observe.
        Err(_) => (
            Reachable::Unknown,
            Some(format!(
                "procStart {recorded_proc_start:?} is not a number — corrupt descriptor"
            )),
        ),
    }
}

/// [`classify_reachability_in`] against the live `/proc`.
pub fn classify_reachability(pid: u32, recorded_proc_start: &str) -> (Reachable, Option<String>) {
    classify_reachability_in(Path::new("/proc"), pid, recorded_proc_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A synthetic stat line: fields after comm are state, ppid, pgrp,
    // session, tty_nr, tpgid, flags, minflt, cminflt, majflt, cmajflt,
    // utime, stime, cutime, cstime, priority, nice, num_threads,
    // itrealvalue, STARTTIME(index 19), ...
    fn stat_line(comm: &str, starttime: u64) -> String {
        format!(
            "42 {comm} S 1 42 42 0 -1 4194304 100 0 0 0 10 5 0 0 20 0 1 0 {starttime} 1234567 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0"
        )
    }

    #[test]
    fn parses_starttime_with_a_plain_comm() {
        assert_eq!(parse_stat_starttime(&stat_line("(node)", 987654)), Some(987654));
    }

    #[test]
    fn parses_starttime_past_a_comm_with_spaces_and_parens() {
        // The trap this parser exists for: comm containing both spaces and
        // parens. A naive whitespace split misindexes everything after comm.
        assert_eq!(
            parse_stat_starttime(&stat_line("(weird (name) here)", 111358845)),
            Some(111358845)
        );
    }

    #[test]
    fn malformed_stat_line_returns_none_rather_than_panicking() {
        assert_eq!(parse_stat_starttime(""), None);
        assert_eq!(parse_stat_starttime("42 (node) S 1"), None);
        assert_eq!(parse_stat_starttime("no parens at all"), None);
        assert_eq!(
            parse_stat_starttime("42 (node) S 1 42 42 0 -1 4194304 100 0 0 0 10 5 0 0 20 0 1 0 notanumber 1234567 100"),
            None,
            "a non-numeric field where starttime belongs must not parse"
        );
    }

    #[test]
    fn real_proc_self_stat_parses() {
        // Anchor against reality: our own /proc/self/stat must parse. If the
        // kernel ever changes this format, this test fails loudly here
        // instead of silently reading every session as Unknown.
        let raw = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat readable");
        assert!(
            parse_stat_starttime(&raw).is_some(),
            "real /proc/self/stat must parse: {raw:?}"
        );
    }

    fn fixture_proc(dir: &Path, pid: u32, starttime: Option<u64>) {
        let pdir = dir.join(pid.to_string());
        std::fs::create_dir_all(&pdir).unwrap();
        if let Some(s) = starttime {
            std::fs::write(pdir.join("stat"), stat_line("(node)", s)).unwrap();
        }
    }

    #[test]
    fn alive_when_starttimes_match() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_proc(tmp.path(), 4242, Some(555));
        let (reach, why) = classify_reachability_in(tmp.path(), 4242, "555");
        assert_eq!(reach, Reachable::Alive);
        assert!(why.is_none());
    }

    #[test]
    fn stale_when_a_reused_pid_has_a_different_starttime() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_proc(tmp.path(), 4242, Some(999));
        let (reach, why) = classify_reachability_in(tmp.path(), 4242, "555");
        assert_eq!(reach, Reachable::Stale);
        assert!(why.is_none());
    }

    #[test]
    fn gone_when_no_such_process() {
        let tmp = tempfile::tempdir().unwrap();
        // No fixture for pid 4242 at all — the stat file read fails NotFound.
        let (reach, why) = classify_reachability_in(tmp.path(), 4242, "555");
        assert_eq!(reach, Reachable::Gone);
        assert!(why.is_none());
    }

    #[test]
    fn unknown_when_stat_is_unparseable() {
        let tmp = tempfile::tempdir().unwrap();
        let pdir = tmp.path().join("4242");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("stat"), "garbage the parser cannot read").unwrap();
        let (reach, why) = classify_reachability_in(tmp.path(), 4242, "555");
        assert_eq!(reach, Reachable::Unknown);
        assert!(why.unwrap().contains("unrecognized format"));
    }

    #[test]
    fn corrupt_recorded_procstart_is_unknown_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_proc(tmp.path(), 4242, Some(555));
        let (reach, why) = classify_reachability_in(tmp.path(), 4242, "not-a-number");
        assert_eq!(reach, Reachable::Unknown);
        assert!(why.unwrap().contains("not a number"));
    }
}
