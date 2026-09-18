//! CLI coverage for `kaijutsu-server rc reseed`'s target-directory
//! resolution (`docs/config-namespace.md`): the real compiled binary against
//! a temporary XDG environment, with no server running.
//!
//! `rc reseed` must resolve the rc tree the same way the server itself
//! would — explicit `--dir` first, then `--mount rc=<dir>`, then
//! `--config-root <dir>/rc`, then the XDG default — never silently falling
//! back to the default while an explicit `--config-root` sits unused.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kaijutsu-server")
}

struct Xdg {
    dir: tempfile::TempDir,
}

impl Xdg {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().unwrap() }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `~/.config/kaijutsu/config/rc` under this fixture's XDG config home —
    /// where reseed lands with no `--config-root`, `--mount`, or `--dir`.
    fn default_rc_dir(&self) -> PathBuf {
        self.path().join("xdg-config/kaijutsu/config/rc")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(server_bin())
            .args(args)
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.path().join("xdg-data"))
            .env("XDG_STATE_HOME", self.path().join("xdg-state"))
            .env("XDG_CACHE_HOME", self.path().join("xdg-cache"))
            .output()
            .expect("run kaijutsu-server")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `--config-root <dir> rc reseed` writes under `<dir>/rc`, resolved the
/// same way the server itself would resolve `/config/rc` from that root —
/// and must NOT also seed the default XDG location, which is the bug this
/// pins: `cmd_rc` used to ignore the parsed global flags entirely.
#[test]
fn config_root_flag_targets_its_own_rc_tree_not_the_default() {
    let xdg = Xdg::new();
    let custom_root = xdg.path().join("custom");
    let expect_rc = custom_root.join("rc");

    let out = xdg.run(&[
        "--config-root",
        custom_root.to_str().unwrap(),
        "rc",
        "reseed",
    ]);
    assert!(out.status.success(), "reseed failed: {}", stderr(&out));

    assert!(
        expect_rc.is_dir() && std::fs::read_dir(&expect_rc).unwrap().next().is_some(),
        "reseed must write under {}: {}",
        expect_rc.display(),
        stdout(&out)
    );
    assert!(
        stdout(&out).contains(expect_rc.to_str().unwrap()),
        "reseed must print the directory it actually wrote to: {}",
        stdout(&out)
    );
    assert!(
        !xdg.default_rc_dir().exists(),
        "reseed must not also write the default XDG rc tree when \
         --config-root was given: {}",
        xdg.default_rc_dir().display()
    );
}

/// `--mount /config/rc=<dir> rc reseed` targets that directory too, the same
/// override a running server would honor for the rc tree specifically.
#[test]
fn mount_flag_targets_its_own_rc_tree() {
    let xdg = Xdg::new();
    let mounted = xdg.path().join("mounted-rc");

    let out = xdg.run(&[
        "--mount",
        &format!("/config/rc={}", mounted.display()),
        "rc",
        "reseed",
    ]);
    assert!(out.status.success(), "reseed failed: {}", stderr(&out));

    assert!(
        mounted.is_dir() && std::fs::read_dir(&mounted).unwrap().next().is_some(),
        "reseed must write under the --mount target {}: {}",
        mounted.display(),
        stdout(&out)
    );
    assert!(
        !xdg.default_rc_dir().exists(),
        "reseed must not also write the default XDG rc tree when \
         --mount rc=<dir> was given: {}",
        xdg.default_rc_dir().display()
    );
}

/// An explicit `--dir` still wins over `--config-root` — the most specific
/// flag beats the more general one, exactly as `docs/config-namespace.md`
/// orders precedence.
#[test]
fn explicit_dir_wins_over_config_root() {
    let xdg = Xdg::new();
    let config_root = xdg.path().join("ignored-root");
    let explicit_dir = xdg.path().join("explicit-rc");

    let out = xdg.run(&[
        "--config-root",
        config_root.to_str().unwrap(),
        "rc",
        "reseed",
        "--dir",
        explicit_dir.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "reseed failed: {}", stderr(&out));

    assert!(
        explicit_dir.is_dir() && std::fs::read_dir(&explicit_dir).unwrap().next().is_some(),
        "reseed must write under the explicit --dir {}: {}",
        explicit_dir.display(),
        stdout(&out)
    );
    assert!(
        !config_root.join("rc").exists(),
        "--dir must win over --config-root's derived rc tree: {}",
        config_root.join("rc").display()
    );
}
