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
    /// `[workspace].rw`: host directories mounted read-write at their own
    /// paths, before any `--rw-mount` flag.
    workspace_rw: Vec<PathBuf>,
    /// `[workspace].create`: make a missing `rw` directory at boot.
    workspace_create: bool,
}

/// The keys `[workspace]` accepts.
const WORKSPACE_KEYS: [&str; 2] = ["rw", "create"];

impl ConfigMounts {
    /// Every tree defaulting to a subdirectory of `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), overrides: BTreeMap::new(), workspace_rw: Vec::new(), workspace_create: false }
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
    /// Its `[workspace]` section, if any, sets [`Self::workspace_rw`] and
    /// [`Self::workspace_creates`].
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
        if let Some(workspace) = parsed.get("workspace") {
            self.load_workspace(workspace).map_err(|e| format!("{}: {e}", path.display()))?;
        }
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

    fn load_workspace(&mut self, value: &toml::Value) -> Result<(), String> {
        let table = value.as_table().ok_or("[workspace] must be a table")?;
        if let Some(key) = table.keys().find(|k| !WORKSPACE_KEYS.contains(&k.as_str())) {
            return Err(format!("[workspace] has no key '{key}' — expected one of {}", WORKSPACE_KEYS.join(", ")));
        }
        let mut rw = Vec::new();
        if let Some(list) = table.get("rw") {
            let list = list.as_array().ok_or("[workspace].rw must be a list of directories")?;
            for dir in list {
                let dir = dir.as_str().ok_or("[workspace].rw must be a list of directories")?;
                rw.push(expand_tilde(PathBuf::from(dir)));
            }
        }
        let create = match table.get("create") {
            Some(value) => value.as_bool().ok_or("[workspace].create must be true or false")?,
            None => false,
        };
        self.workspace_rw = rw;
        self.workspace_create = create;
        Ok(())
    }

    /// The read-write host directories `[workspace].rw` declares, in order.
    pub fn workspace_rw(&self) -> &[PathBuf] {
        &self.workspace_rw
    }

    /// Whether `[workspace].create` asks for a missing directory to be made.
    pub fn workspace_creates(&self) -> bool {
        self.workspace_create
    }

    /// Make each missing `[workspace].rw` directory when `create = true`.
    /// Run before the mounts are validated. With `create = false` this does
    /// nothing, and validation refuses a missing directory by name.
    pub fn create_workspace(&self) -> Result<(), String> {
        if !self.workspace_create {
            return Ok(());
        }
        for dir in &self.workspace_rw {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("[workspace] could not create {}: {e}", dir.display()))?;
        }
        Ok(())
    }

    /// The kernel's read-write mounts: `[workspace].rw`, then `flags`
    /// (`--rw-mount`, or solo-acp's `--mount`). Makes missing declared
    /// directories first when `create = true`. The caller validates the list.
    pub fn rw_mounts_with(&self, flags: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
        self.create_workspace()?;
        Ok(self.workspace_rw.iter().chain(flags).cloned().collect())
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

    /// `[workspace]` declares read-write host directories, mounted at their
    /// own paths, in the same file as the config trees. A file with only a
    /// workspace section still loads.
    #[test]
    fn a_workspace_section_declares_read_write_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = ConfigMounts::new(dir.path());
        assert!(m.workspace_rw().is_empty(), "nothing declared, nothing mounted");
        std::fs::write(
            dir.path().join("mounts.toml"),
            "[workspace]\nrw = [\"/app\", \"/git\", \"/srv\"]\n",
        )
        .unwrap();
        m.load_declarations().unwrap();
        assert_eq!(m.workspace_rw(), [PathBuf::from("/app"), PathBuf::from("/git"), PathBuf::from("/srv")]);
        assert!(!m.workspace_creates(), "a missing directory is refused unless asked for");
        assert_eq!(m.host_dir(RC_ROOT), dir.path().join("rc"), "the trees are untouched");
    }

    /// A typo in `[workspace]` fails the boot and names the valid keys, the
    /// same rule as an unknown tree.
    #[test]
    fn a_malformed_workspace_section_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = ConfigMounts::new(dir.path());
        for (text, why) in [
            ("[workspace]\nrws = [\"/app\"]\n", "an unknown key"),
            ("[workspace]\nrw = \"/app\"\n", "rw is a list, not a string"),
            ("[workspace]\nrw = [1]\n", "rw holds strings"),
            ("[workspace]\ncreate = \"yes\"\n", "create is a boolean"),
        ] {
            std::fs::write(dir.path().join("mounts.toml"), text).unwrap();
            let err = m.load_declarations().expect_err(why);
            assert!(err.contains("workspace"), "{why}: {err}");
        }
        std::fs::write(dir.path().join("mounts.toml"), "[workspace]\nrws = []\n").unwrap();
        let err = m.load_declarations().unwrap_err();
        assert!(err.contains("rw") && err.contains("create"), "the error lists the valid keys: {err}");
    }

    /// `create = true` makes each missing workspace directory before the
    /// kernel validates its mounts; without it nothing is created.
    #[test]
    fn create_makes_missing_workspace_directories_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("git");
        let toml = |create: bool| format!("[workspace]\nrw = [{:?}]\ncreate = {create}\n", missing.to_str().unwrap());

        let mut m = ConfigMounts::new(dir.path());
        std::fs::write(dir.path().join("mounts.toml"), toml(false)).unwrap();
        m.load_declarations().unwrap();
        m.create_workspace().unwrap();
        assert!(!missing.exists(), "create = false leaves a missing directory to be refused");

        let mut m = ConfigMounts::new(dir.path());
        std::fs::write(dir.path().join("mounts.toml"), toml(true)).unwrap();
        m.load_declarations().unwrap();
        assert!(m.workspace_creates());
        m.create_workspace().unwrap();
        assert!(missing.is_dir(), "create = true makes it");
        m.create_workspace().unwrap();
    }

    /// A kernel's read-write mounts are the file's, then the flags'. Both
    /// boot paths ask for this one list, after any directory was made.
    #[test]
    fn read_write_mounts_join_the_file_and_the_flags() {
        let dir = tempfile::tempdir().unwrap();
        let declared = dir.path().join("srv");
        std::fs::write(
            dir.path().join("mounts.toml"),
            format!("[workspace]\nrw = [{:?}]\ncreate = true\n", declared.to_str().unwrap()),
        )
        .unwrap();
        let mut m = ConfigMounts::new(dir.path());
        m.load_declarations().unwrap();
        let flag = PathBuf::from("/from/a/flag");
        let rw = m.rw_mounts_with(std::slice::from_ref(&flag)).unwrap();
        assert_eq!(rw, [declared.clone(), flag]);
        assert!(declared.is_dir(), "the list is ready to validate: declared directories exist");
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
