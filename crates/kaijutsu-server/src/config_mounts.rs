//! Where each `/config` tree comes from on the host.
//!
//! A `/config` path is a well-known *name*; the directory behind it is a
//! declaration. `docs/config-namespace.md` is canonical.
//!
//! Precedence, highest first:
//!
//! ```text
//! kaijutsu-server --mount /config/rc=/home/amy/src/kaijutsu/assets/defaults/rc
//! <config-root>/mounts.toml
//! <config-root>/<name>            # the default: an ordinary subdirectory
//! ```
//!
//! There is no bootstrap cycle: the root arrives from a flag or the default,
//! and `mounts.toml` inside it only ever names its own siblings.
//!
//! **Declare nothing and nothing looks special.** Every tree is a
//! subdirectory of one host directory and this module is invisible; it earns
//! its keep the first time a tree diverges — pointing `/config/rc` at a
//! checkout so an edit is live without a rebuild, or a test pointing a tree at
//! a tempdir through the same mechanism production uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kaijutsu_types::paths::{self, CONFIG_TREES};

/// The resolved host directory for every config tree.
#[derive(Debug, Clone)]
pub struct ConfigMounts {
    root: PathBuf,
    overrides: BTreeMap<&'static str, PathBuf>,
}

impl ConfigMounts {
    /// Every tree defaulting to a subdirectory of `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), overrides: BTreeMap::new() }
    }

    /// `~/.config/kaijutsu/config` — the one place the default config root is
    /// decided. Everything else takes the path it is handed.
    pub fn default_root() -> PathBuf {
        kaish_kernel::xdg_config_home().join("kaijutsu").join("config")
    }

    /// Point one tree at a host directory.
    ///
    /// `tree_root` must be one of [`CONFIG_TREES`]; an unknown path is a
    /// typo in a flag or a config file, and reporting it beats silently
    /// declaring a mount nothing reads.
    pub fn set(&mut self, tree_root: &str, host: impl Into<PathBuf>) -> Result<(), String> {
        let known = CONFIG_TREES
            .into_iter()
            .find(|t| *t == tree_root)
            .ok_or_else(|| {
                format!(
                    "unknown config tree '{tree_root}' — expected one of {}",
                    CONFIG_TREES.join(", ")
                )
            })?;
        self.overrides.insert(known, expand_tilde(host.into()));
        Ok(())
    }

    /// Parse and apply one `--mount <tree>=<dir>` argument.
    pub fn set_from_arg(&mut self, arg: &str) -> Result<(), String> {
        let (tree, dir) = arg
            .split_once('=')
            .ok_or_else(|| format!("--mount needs <tree>=<dir>, got '{arg}'"))?;
        if dir.trim().is_empty() {
            return Err(format!("--mount {tree}= needs a directory"));
        }
        self.set(tree.trim(), dir.trim())
    }

    /// Apply `<root>/mounts.toml` if it exists. Returns how many trees it
    /// repointed; `Ok(0)` when the file is absent, which is the normal case.
    ///
    /// A malformed file is an error, never a silent skip: it was written on
    /// purpose, and booting with mounts the operator did not ask for is worse
    /// than not booting.
    pub fn load_declarations(&mut self) -> Result<usize, String> {
        let path = self.root.join("mounts.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let parsed: toml::Value = text
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let Some(table) = parsed.get("mounts").and_then(|m| m.as_table()) else {
            return Ok(0);
        };
        let mut n = 0;
        for (tree, value) in table {
            let dir = value
                .as_str()
                .ok_or_else(|| format!("{}: [mounts].\"{tree}\" must be a string", path.display()))?;
            self.set(tree, dir)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            n += 1;
        }
        Ok(n)
    }

    /// The host directory backing one tree.
    pub fn host_dir(&self, tree_root: &str) -> PathBuf {
        if let Some(dir) = self.overrides.get(tree_root) {
            return dir.clone();
        }
        let name = paths::config_tree_name(tree_root)
            .expect("host_dir takes a CONFIG_TREES member");
        self.root.join(name)
    }

    /// Whether this tree was pointed somewhere other than its default. Worth
    /// saying out loud at boot — a tree read from an unexpected place is the
    /// fact hardest to reconstruct later from a running kernel.
    pub fn is_declared(&self, tree_root: &str) -> bool {
        self.overrides.contains_key(tree_root)
    }

    /// Every tree and its host directory, in [`CONFIG_TREES`] order.
    pub fn resolved(&self) -> Vec<(&'static str, PathBuf)> {
        CONFIG_TREES
            .into_iter()
            .map(|tree| (tree, self.host_dir(tree)))
            .collect()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Expand a leading `~` against the home directory. A declaration is written
/// by a human in a file or a flag, where `~` is what a human types; nothing
/// else in a path is interpreted.
fn expand_tilde(path: PathBuf) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path;
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::paths::{CLIENT_ROOT, CONFIG_ROOT, MIDI_ROOT, RC_ROOT};

    /// Declare nothing and every tree is an ordinary subdirectory of one
    /// root. This is the case that must stay boring — the registry is
    /// invisible until something diverges.
    #[test]
    fn undeclared_trees_are_plain_subdirectories_of_the_root() {
        let m = ConfigMounts::new("/srv/kj");
        assert_eq!(m.host_dir(RC_ROOT), PathBuf::from("/srv/kj/rc"));
        assert_eq!(m.host_dir(CONFIG_ROOT), PathBuf::from("/srv/kj/kernel"));
        assert_eq!(m.host_dir(CLIENT_ROOT), PathBuf::from("/srv/kj/client"));
        assert_eq!(m.host_dir(MIDI_ROOT), PathBuf::from("/srv/kj/midi"));
        assert!(CONFIG_TREES.iter().all(|t| !m.is_declared(t)));
    }

    /// One tree diverges and the others do not move. The point of the
    /// registry: `/config/rc` at a checkout, everything else where it was.
    #[test]
    fn one_declared_tree_leaves_the_others_alone() {
        let mut m = ConfigMounts::new("/srv/kj");
        m.set(RC_ROOT, "/home/amy/src/kaijutsu/assets/defaults/rc").unwrap();
        assert_eq!(
            m.host_dir(RC_ROOT),
            PathBuf::from("/home/amy/src/kaijutsu/assets/defaults/rc")
        );
        assert_eq!(m.host_dir(MIDI_ROOT), PathBuf::from("/srv/kj/midi"));
        assert!(m.is_declared(RC_ROOT));
        assert!(!m.is_declared(MIDI_ROOT));
    }

    /// An unknown tree is a typo, and a typo that boots is worse than one
    /// that does not: a silently-ignored declaration means the kernel reads
    /// a directory the operator believes it does not.
    #[test]
    fn an_unknown_tree_is_refused_and_names_the_valid_ones() {
        let mut m = ConfigMounts::new("/srv/kj");
        let err = m.set("/config/rcc", "/tmp/x").unwrap_err();
        assert!(err.contains("/config/rcc"), "{err}");
        assert!(err.contains(RC_ROOT), "the error must list what IS valid: {err}");
        // `/etc/rc` is the pre-melt spelling and must not quietly work.
        assert!(m.set("/etc/rc", "/tmp/x").is_err());
    }

    #[test]
    fn mount_args_parse_and_reject_malformed_ones() {
        let mut m = ConfigMounts::new("/srv/kj");
        m.set_from_arg("/config/midi=/tmp/gear").unwrap();
        assert_eq!(m.host_dir(MIDI_ROOT), PathBuf::from("/tmp/gear"));
        assert!(m.set_from_arg("/config/midi").is_err(), "no '=' is malformed");
        assert!(m.set_from_arg("/config/midi=").is_err(), "empty dir is malformed");
    }

    /// `mounts.toml` repoints trees, and its absence is the normal case
    /// rather than an error.
    #[test]
    fn declarations_load_from_the_root_and_absence_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = ConfigMounts::new(dir.path());
        assert_eq!(m.load_declarations().unwrap(), 0, "no file is not an error");

        std::fs::write(
            dir.path().join("mounts.toml"),
            "[mounts]\n\"/config/rc\" = \"/tmp/rc-elsewhere\"\n",
        )
        .unwrap();
        assert_eq!(m.load_declarations().unwrap(), 1);
        assert_eq!(m.host_dir(RC_ROOT), PathBuf::from("/tmp/rc-elsewhere"));
        assert_eq!(m.host_dir(CLIENT_ROOT), dir.path().join("client"));
    }

    /// A malformed declarations file fails the boot rather than being
    /// skipped. It was written on purpose; mounting something else instead is
    /// the silent-fallback failure this repo refuses.
    ///
    /// Falsified by treating a parse error as `Ok(0)`: both asserts trip.
    #[test]
    fn a_malformed_declarations_file_is_an_error_not_a_skip() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = ConfigMounts::new(dir.path());

        std::fs::write(dir.path().join("mounts.toml"), "this is not toml {{{").unwrap();
        assert!(m.load_declarations().is_err(), "unparseable toml must fail loud");

        std::fs::write(
            dir.path().join("mounts.toml"),
            "[mounts]\n\"/config/nope\" = \"/tmp/x\"\n",
        )
        .unwrap();
        assert!(m.load_declarations().is_err(), "an unknown tree must fail loud");
    }

    /// `~` is what a human writes in a config file, so a declaration expands
    /// it. Nothing else in the path is interpreted.
    #[test]
    fn a_leading_tilde_expands_and_nothing_else_is_interpreted() {
        // SAFETY: single-threaded test, restored immediately after.
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/home/testuser") };
        let mut m = ConfigMounts::new("/srv/kj");
        m.set(RC_ROOT, "~/rc").unwrap();
        assert_eq!(m.host_dir(RC_ROOT), PathBuf::from("/home/testuser/rc"));
        m.set(MIDI_ROOT, "/opt/~weird/midi").unwrap();
        assert_eq!(
            m.host_dir(MIDI_ROOT),
            PathBuf::from("/opt/~weird/midi"),
            "a tilde that is not the first component is an ordinary character"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// `resolved()` covers every tree, so a caller that mounts what it
    /// returns cannot miss one — a fifth tree added to `CONFIG_TREES` is
    /// mounted without touching the bootstrap.
    #[test]
    fn resolved_covers_every_config_tree() {
        let m = ConfigMounts::new("/srv/kj");
        let resolved = m.resolved();
        assert_eq!(resolved.len(), CONFIG_TREES.len());
        for tree in CONFIG_TREES {
            assert!(resolved.iter().any(|(t, _)| *t == tree), "missing {tree}");
        }
    }
}
