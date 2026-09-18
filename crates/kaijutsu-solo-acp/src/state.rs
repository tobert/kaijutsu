//! The solo kernel's state directory, its key, and the rows a first turn
//! needs.
//!
//! Everything a solo kernel writes lives under one directory: `kernel.db`,
//! `auth.db`, the SSH host key, the connecting character's key, and the
//! `/config` trees. Nothing here reads or writes the operator's XDG
//! locations — every path is named explicitly, so a solo run cannot reach
//! the kernel a person is already using.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use russh::keys::{PrivateKey, ssh_key};

use kaijutsu_kernel::kernel_db::{BackendRow, CharacterRow, KernelDb};
use kaijutsu_kernel::seed_backends;
use kaijutsu_server::AuthDb;
use kaijutsu_types::{BackendId, PrincipalId};

use crate::provider::ModelChoice;

/// The temporary state directory this process owns, if it made one.
///
/// A process-wide registry rather than a guard on a value, because a guard
/// only runs where a value is dropped, and this process exits from more than
/// one place: an ACP client disconnecting, a signal the kernel's own handler
/// answers, and a kernel that fails or panics while serving. Two of those
/// call `exit()` from another thread, which runs no destructors. Registering
/// with `atexit` puts the removal after every one of them, so the directory
/// holding a generated private key outlives no orderly exit.
static TEMP_STATE: OnceLock<PathBuf> = OnceLock::new();
static TEMP_STATE_GONE: AtomicBool = AtomicBool::new(false);

/// Remove the temporary state directory, once, whichever exit path arrives
/// first. A named `--state-dir` is never registered here, so this is a no-op
/// for one.
pub fn remove_temp_state() -> Result<()> {
    let Some(path) = TEMP_STATE.get() else {
        return Ok(());
    };
    if TEMP_STATE_GONE.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    fs::remove_dir_all(path)
        .with_context(|| format!("remove the temporary state directory {}", path.display()))
}

/// The `atexit` hook. It cannot return a failure to anyone, so it says so on
/// stderr: a private key left behind is worth a line.
extern "C" fn remove_temp_state_at_exit() {
    if let Err(e) = remove_temp_state() {
        eprintln!("kaijutsu-solo-acp: {e:#}");
    }
}

/// Where a solo kernel keeps its state.
pub struct SoloState {
    root: PathBuf,
    /// Whether this directory is ours to remove at exit.
    temporary: bool,
}

impl SoloState {
    /// Use `state_dir` if given, else a fresh temporary directory removed at
    /// exit.
    pub fn prepare(state_dir: Option<PathBuf>) -> Result<Self> {
        match state_dir {
            Some(root) => {
                refuse_operator_state_dir(&root)?;
                fs::create_dir_all(&root)
                    .with_context(|| format!("create the state directory {}", root.display()))?;
                Ok(Self {
                    root,
                    temporary: false,
                })
            }
            None => {
                let temp = tempfile::Builder::new()
                    .prefix("kaijutsu-solo-")
                    .tempdir()
                    .context("create a temporary state directory")?;
                // Take the path out of the guard: removal belongs to the
                // registry above, which covers every exit, and two owners
                // would be one owner too many.
                let root = temp.keep();
                if TEMP_STATE.set(root.clone()).is_ok() {
                    // SAFETY: registering a plain function once, before any
                    // exit path can run. The hook touches only the statics
                    // above.
                    unsafe { libc::atexit(remove_temp_state_at_exit) };
                }
                Ok(Self {
                    root,
                    temporary: true,
                })
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_root(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The private key the ACP bridge connects with.
    pub fn client_key_path(&self) -> PathBuf {
        self.root.join("keys").join("solo")
    }

    pub fn host_key_path(&self) -> PathBuf {
        self.root.join("host_key")
    }

    pub fn kernel_db_path(&self) -> PathBuf {
        self.root.join("kernel.db")
    }

    pub fn auth_db_path(&self) -> PathBuf {
        self.root.join("auth.db")
    }

    /// Remove a temporary directory now, on the orderly path, so a failure
    /// is reported rather than swallowed by the exit hook.
    pub fn clean_up(&mut self) -> Result<()> {
        if !self.temporary {
            return Ok(());
        }
        remove_temp_state()
    }
}

/// Refuse a state directory inside the operator's own kaijutsu trees.
///
/// A solo kernel is a separate kernel. Pointed at `~/.local/share/kaijutsu`
/// or `~/.config/kaijutsu` it would open the databases a running kernel owns
/// and seed its own root character into them, so this refuses before
/// anything is created.
fn refuse_operator_state_dir(root: &Path) -> Result<()> {
    for operator in [
        kaish_kernel::xdg_data_home().join("kaijutsu"),
        kaish_kernel::xdg_config_home().join("kaijutsu"),
    ] {
        if root.starts_with(&operator) {
            anyhow::bail!(
                "--state-dir {} is inside the operator's kaijutsu at {}; a solo kernel keeps                  its own state somewhere else",
                root.display(),
                operator.display()
            );
        }
    }
    Ok(())
}

/// Load the connecting character's key, generating it on first run.
///
/// The private key stays at mode 0600 and never leaves this directory; the
/// ACP bridge reads it back through `KeySource::from_file`.
pub fn ensure_client_key(path: &Path) -> Result<PrivateKey> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create the key directory {}", parent.display()))?;
    }
    if path.exists() {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read the key {}", path.display()))?;
        return PrivateKey::from_openssh(&text)
            .with_context(|| format!("parse the key {}", path.display()));
    }

    let key = PrivateKey::random(&mut rand_v10::rng(), russh::keys::Algorithm::Ed25519)
        .map_err(|e| anyhow::anyhow!("generate an Ed25519 key: {e}"))?;
    let openssh = key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(|e| anyhow::anyhow!("serialize the key: {e}"))?;
    fs::write(path, openssh.as_bytes())
        .with_context(|| format!("write the key {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict the key {}", path.display()))?;
    }
    let public = path.with_extension("pub");
    fs::write(
        &public,
        key.public_key()
            .to_openssh()
            .map_err(|e| anyhow::anyhow!("serialize the public key: {e}"))?,
    )
    .with_context(|| format!("write {}", public.display()))?;
    Ok(key)
}

/// Write the identity and model rows a first turn needs, before the kernel
/// opens the same database.
///
/// - `root_character` is the connecting identity, and the reviewer every
///   approval resolves to.
/// - `performer` is the character each ACP session's context is played by. A
///   model turn needs a live performer distinct from its reviewer
///   (`docs/approval-identity.md`), so these are two characters, never one.
/// - The model rows start from the kernel's own factory floor, so a solo
///   kernel's providers, context windows, and tunables are the ones every
///   other kernel ships.
/// - `max_tokens`, when given, overrides the factory output-token ceiling
///   (`kaijutsu_kernel::seed_backends::FACTORY_MAX_TOKENS`) in the written
///   defaults row. Left `None`, the factory ceiling from
///   `ensure_factory_backends` stands.
pub fn prepare_rows(
    state: &SoloState,
    root_character: &str,
    performer: &str,
    key: &PrivateKey,
    choice: &ModelChoice,
    max_tokens: Option<i64>,
) -> Result<()> {
    let mut db = KernelDb::open(state.kernel_db_path())
        .with_context(|| format!("open {}", state.kernel_db_path().display()))?;
    let auth = AuthDb::open(state.auth_db_path())
        .with_context(|| format!("open {}", state.auth_db_path().display()))?;

    kaijutsu_server::init::init_root(
        &db,
        &auth,
        root_character,
        key.public_key(),
        Some("kaijutsu-solo-acp"),
    )
    .map_err(|e| anyhow::anyhow!("make {root_character} the root character: {e}"))?;

    seed_backends::ensure_factory_backends(&mut db, PrincipalId::system())
        .context("seed the factory backends")?;

    if choice.write_backend_row {
        let existing = db
            .get_backend_by_name(&choice.backend)
            .with_context(|| format!("read the {} backend row", choice.backend))?;
        let row = BackendRow {
            backend_id: existing.map_or_else(BackendId::new, |row| row.backend_id),
            name: choice.backend.clone(),
            kind: choice.kind.clone(),
            base_url: choice.base_url.clone(),
            api_key_env: choice.api_key_env.clone(),
            api_key_file: None,
            key_optional: choice.key_optional,
            request_timeout_secs: None,
            idle_timeout_secs: None,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::system(),
        };
        db.upsert_backend(&row)
            .with_context(|| format!("write the {} backend row", choice.backend))?;
    }

    let mut defaults = db
        .get_llm_defaults()
        .context("read the model defaults")?
        .context("the factory floor wrote no model defaults")?;
    defaults.default_backend = choice.backend.clone();
    defaults.default_model = choice.model.clone();
    if let Some(max_tokens) = max_tokens {
        defaults.max_tokens = Some(max_tokens);
    }
    db.set_llm_defaults(&defaults)
        .context("point the model defaults at the chosen provider")?;

    if db
        .get_character_by_name(performer)
        .with_context(|| format!("read the character {performer}"))?
        .is_none()
    {
        db.insert_character(&CharacterRow {
            principal_id: PrincipalId::new(),
            name: performer.to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None,
            root_ctx: None,
            root: false,
        })
        .with_context(|| format!("create the character {performer}"))?;
    }

    Ok(())
}

/// Install a gate policy file into the solo kernel's `/config/kernel`.
///
/// Copied verbatim: the gate file is read at every gated submission, so a
/// solo run's policy is whatever the caller handed us.
pub fn install_gate_config(state: &SoloState, source: &Path) -> Result<()> {
    let body = fs::read_to_string(source)
        .with_context(|| format!("read the gate policy {}", source.display()))?;
    let dir = state.config_root().join("kernel");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let target = dir.join("gate.toml");
    fs::write(&target, body).with_context(|| format!("write {}", target.display()))
}

/// Apply an rc overlay onto the seeded `/config/rc` tree: every regular file
/// under `overlay`, except a top-level `README.md`, replaces the file at the
/// same relative path under this kernel's rc tree.
///
/// The kernel seeds `/config/rc` on the way up (`kernel::start`), and rc
/// scripts are read fresh from disk at every lifecycle run rather than
/// cached at boot (`kaijutsu_kernel::rc::load_scripts`), so this needs to
/// land only before the first context of an overlaid type is created — the
/// caller runs it after `kernel::start` and before the ACP bridge serves.
///
/// The destination is unlinked before the copy, never written through: a
/// reseed writes some rc entries (`coder/create/S00-base.kai` and the like)
/// as symlinks into `lib/`, and copying over a symlink would edit the shared
/// base every other context type reads.
///
/// Refuses, before replacing anything, when: `overlay` is missing or not a
/// directory; it holds no file to apply; an entry anywhere under it is a
/// symlink or another non-regular file (this also catches a symlinked
/// directory that would otherwise let a walk escape the rc tree, since a
/// symlink is refused before it is ever followed); or a file's destination
/// parent directory does not already exist in the seeded tree — a typo'd
/// type or verb name must refuse rather than create a new one.
pub fn install_rc_overlay(state: &SoloState, overlay: &Path) -> Result<()> {
    if !overlay.exists() {
        anyhow::bail!("--rc-overlay {} does not exist", overlay.display());
    }
    if !overlay.is_dir() {
        anyhow::bail!("--rc-overlay {} is not a directory", overlay.display());
    }

    let mut relative_files = Vec::new();
    collect_overlay_files(overlay, overlay, &mut relative_files)?;
    if relative_files.is_empty() {
        anyhow::bail!(
            "--rc-overlay {} has nothing to apply (only a top-level README.md, or no files \
             at all)",
            overlay.display()
        );
    }

    let rc_root = state.config_root().join("rc");
    for relative in &relative_files {
        let src = overlay.join(relative);
        let dest = rc_root.join(relative);
        if !dest.starts_with(&rc_root) {
            anyhow::bail!(
                "--rc-overlay entry {} would land outside the rc tree",
                relative.display()
            );
        }
        let parent = dest.parent().with_context(|| {
            format!("--rc-overlay entry {} has no parent path", relative.display())
        })?;
        if !parent.is_dir() {
            anyhow::bail!(
                "--rc-overlay entry {} has no seeded directory at {} — check the type and \
                 verb names",
                relative.display(),
                parent.display(),
            );
        }

        match fs::remove_file(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("remove {}", dest.display()));
            }
        }
        fs::copy(&src, &dest)
            .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
        eprintln!("kaijutsu-solo-acp: rc overlay: {} replaced", relative.display());
    }
    Ok(())
}

/// Recursively collect `dir`'s regular files, relative to `root`, refusing a
/// symlink or any other non-regular entry anywhere under it. A top-level
/// `README.md` (directly inside `root`) is skipped; a nested one is not.
fn collect_overlay_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked from root, so every path is under it")
            .to_path_buf();
        if dir == root && relative == Path::new("README.md") {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_symlink() {
            anyhow::bail!(
                "--rc-overlay entry {} is a symlink; only regular files are allowed",
                relative.display()
            );
        } else if file_type.is_dir() {
            collect_overlay_files(root, &path, out)?;
        } else if file_type.is_file() {
            out.push(relative);
        } else {
            anyhow::bail!(
                "--rc-overlay entry {} is neither a regular file nor a directory",
                relative.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temporary_state_directory_goes_away_when_asked() {
        let mut state = SoloState::prepare(None).expect("prepare temp state");
        let path = state.root().to_path_buf();
        assert!(path.is_dir());
        state.clean_up().expect("clean up");
        assert!(!path.exists(), "{} should be gone", path.display());
    }

    #[test]
    fn a_named_state_directory_is_kept() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("solo");
        let mut state = SoloState::prepare(Some(root.clone())).expect("prepare named state");
        state.clean_up().expect("clean up leaves a named directory alone");
        assert!(root.is_dir(), "a named directory survives");
    }

    #[test]
    fn the_operators_own_trees_are_refused() {
        for operator in [
            kaish_kernel::xdg_data_home().join("kaijutsu"),
            kaish_kernel::xdg_config_home().join("kaijutsu"),
        ] {
            let inside = operator.join("kernel");
            let error = refuse_operator_state_dir(&inside)
                .expect_err("the operator's kernel is not a solo one");
            assert!(error.to_string().contains(&inside.display().to_string()));
            assert!(!inside.exists() || inside.is_dir(), "nothing was created");
        }
        let elsewhere = std::env::temp_dir().join("kaijutsu-solo-somewhere");
        refuse_operator_state_dir(&elsewhere).expect("an ordinary directory is fine");
    }

    /// A choice that reuses the `deepseek` factory backend row, so
    /// `prepare_rows` needs to write nothing but the defaults and the
    /// performer character.
    fn factory_choice() -> ModelChoice {
        ModelChoice {
            backend: "deepseek".to_string(),
            kind: "deepseek".to_string(),
            model: "deepseek-v4-flash".to_string(),
            base_url: None,
            api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
            key_optional: false,
            write_backend_row: false,
        }
    }

    /// A named state directory rather than `SoloState::prepare(None)`: a
    /// temporary one registers itself in the process-wide `TEMP_STATE`
    /// singleton (`remove_temp_state`), which only one directory per test
    /// binary can own. Two temporary states in the same run would race to
    /// register and `clean_up` could remove a sibling test's directory
    /// instead of its own. A named directory sidesteps that registry and
    /// tempfile's own `TempDir` drop cleans it up here.
    fn named_state() -> (tempfile::TempDir, SoloState) {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("solo");
        let state = SoloState::prepare(Some(root)).expect("prepare named state");
        (parent, state)
    }

    #[test]
    fn a_max_tokens_override_lands_in_the_defaults_row() {
        let (_parent, state) = named_state();
        let key = ensure_client_key(&state.client_key_path()).expect("generate the client key");
        prepare_rows(&state, "solo", "solo-coder", &key, &factory_choice(), Some(4096))
            .expect("prepare rows with an override");

        let db = KernelDb::open(state.kernel_db_path()).expect("reopen the kernel db");
        let defaults = db
            .get_llm_defaults()
            .expect("read the model defaults")
            .expect("prepare_rows wrote a defaults row");
        assert_eq!(defaults.max_tokens, Some(4096));
    }

    #[test]
    fn without_an_override_the_factory_ceiling_stands() {
        let (_parent, state) = named_state();
        let key = ensure_client_key(&state.client_key_path()).expect("generate the client key");
        prepare_rows(&state, "solo", "solo-coder", &key, &factory_choice(), None)
            .expect("prepare rows with no override");

        let db = KernelDb::open(state.kernel_db_path()).expect("reopen the kernel db");
        let defaults = db
            .get_llm_defaults()
            .expect("read the model defaults")
            .expect("prepare_rows wrote a defaults row");
        // `seed_backends::FACTORY_MAX_TOKENS` is private to that module; this
        // is docs/solo-acp.md's documented default, kept in sync by hand.
        assert_eq!(defaults.max_tokens, Some(16384));
    }

    #[test]
    fn a_generated_key_is_reloaded_rather_than_replaced() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("keys").join("solo");
        let first = ensure_client_key(&path).expect("generate");
        let second = ensure_client_key(&path).expect("reload");
        assert_eq!(
            first.public_key().to_openssh().expect("first public"),
            second.public_key().to_openssh().expect("second public"),
            "a second run must keep the identity the kernel already trusts"
        );
        assert!(path.with_extension("pub").is_file());
    }

    // ---- rc overlay -------------------------------------------------------

    /// A minimal stand-in for what a real reseed leaves under `<state>/config/
    /// rc`: `coder/create/S00-base.kai` linked to a shared script under
    /// `lib/`, and one real (non-linked) `coder/create/S00-stance.kai`.
    /// Returns the state and the path of the shared `lib` script, so a test
    /// can prove it was not written through.
    fn seeded_rc_tree() -> (tempfile::TempDir, SoloState, PathBuf) {
        let (parent, state) = named_state();
        let rc = state.config_root().join("rc");
        let lib_dir = rc.join("lib").join("create");
        let coder_dir = rc.join("coder").join("create");
        fs::create_dir_all(&lib_dir).expect("create the lib dir");
        fs::create_dir_all(&coder_dir).expect("create the coder dir");

        let lib_script = lib_dir.join("S00-base.kai");
        fs::write(&lib_script, "# shared base script\n").expect("write the shared base");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&lib_script, coder_dir.join("S00-base.kai"))
            .expect("link coder/create/S00-base.kai to the shared base");

        fs::write(coder_dir.join("S00-stance.kai"), "# shipped stance\n")
            .expect("write the shipped stance");

        (parent, state, lib_script)
    }

    /// An overlay directory under the test's own scratch parent (never
    /// `/tmp`), holding a top-level `README.md` (must be skipped) plus
    /// `coder/create/S00-base.kai` and a brand-new `coder/create/S00-base.md`
    /// companion.
    fn write_overlay(dir: &Path) {
        let coder_dir = dir.join("coder").join("create");
        fs::create_dir_all(&coder_dir).expect("create the overlay's coder dir");
        fs::write(dir.join("README.md"), "not applied\n").expect("write the overlay README");
        fs::write(coder_dir.join("S00-base.kai"), "# overlay base script\n")
            .expect("write the overlay base script");
        fs::write(coder_dir.join("S00-base.md"), "overlay marker prose\n")
            .expect("write the overlay base companion");
    }

    #[test]
    fn an_overlay_replaces_a_symlink_without_writing_through_it() {
        let (_seed_parent, state, lib_script) = seeded_rc_tree();
        let lib_before = fs::read(&lib_script).expect("read the shared base before");

        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("variant");
        write_overlay(&overlay);

        install_rc_overlay(&state, &overlay).expect("apply the overlay");

        let dest = state.config_root().join("rc").join("coder").join("create").join("S00-base.kai");
        assert!(
            !fs::symlink_metadata(&dest).expect("stat the destination").file_type().is_symlink(),
            "the destination must be a plain file, not the seeded symlink"
        );
        assert_eq!(
            fs::read_to_string(&dest).expect("read the replaced file"),
            "# overlay base script\n"
        );

        let lib_after = fs::read(&lib_script).expect("read the shared base after");
        assert_eq!(
            lib_before, lib_after,
            "unlinking the symlink first must leave the shared lib script untouched"
        );

        let companion = state
            .config_root()
            .join("rc")
            .join("coder")
            .join("create")
            .join("S00-base.md");
        assert_eq!(
            fs::read_to_string(&companion).expect("read the new companion"),
            "overlay marker prose\n",
            "a file absent from the seeded tree is still written, given its parent dir exists"
        );

        let readme = state.config_root().join("rc").join("README.md");
        assert!(!readme.exists(), "a top-level README.md must never be applied");
    }

    #[test]
    fn reapplying_the_same_overlay_is_idempotent() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("variant");
        write_overlay(&overlay);

        install_rc_overlay(&state, &overlay).expect("first apply");
        install_rc_overlay(&state, &overlay).expect("second apply must succeed the same way");

        let dest = state.config_root().join("rc").join("coder").join("create").join("S00-base.kai");
        assert_eq!(
            fs::read_to_string(&dest).expect("read the replaced file"),
            "# overlay base script\n"
        );
    }

    #[test]
    fn a_missing_overlay_directory_refuses() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let missing = tempfile::tempdir().expect("parent").path().join("nowhere");
        let error = install_rc_overlay(&state, &missing).expect_err("a missing dir must refuse");
        assert!(error.to_string().contains(&missing.display().to_string()), "{error}");
    }

    #[test]
    fn a_plain_file_as_the_overlay_refuses() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let file_parent = tempfile::tempdir().expect("parent");
        let file = file_parent.path().join("not-a-dir");
        fs::write(&file, "nope\n").expect("write a plain file");
        let error = install_rc_overlay(&state, &file).expect_err("a file must refuse");
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn an_empty_overlay_refuses() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("empty");
        fs::create_dir_all(&overlay).expect("create the empty overlay");
        let error = install_rc_overlay(&state, &overlay).expect_err("an empty dir must refuse");
        assert!(error.to_string().contains("nothing to apply"), "{error}");
    }

    #[test]
    fn an_overlay_with_only_a_readme_refuses() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("readme-only");
        fs::create_dir_all(&overlay).expect("create the overlay");
        fs::write(overlay.join("README.md"), "just docs\n").expect("write the readme");
        let error = install_rc_overlay(&state, &overlay).expect_err("README-only must refuse");
        assert!(error.to_string().contains("nothing to apply"), "{error}");
    }

    #[test]
    fn a_symlink_inside_the_overlay_refuses() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("variant");
        let coder_dir = overlay.join("coder").join("create");
        fs::create_dir_all(&coder_dir).expect("create the overlay's coder dir");
        let outside = overlay_parent.path().join("outside.kai");
        fs::write(&outside, "# not from the overlay\n").expect("write a file outside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, coder_dir.join("S00-base.kai"))
            .expect("symlink into the overlay");

        let error = install_rc_overlay(&state, &overlay).expect_err("a symlink must refuse");
        assert!(error.to_string().contains("symlink"), "{error}");

        let dest = state.config_root().join("rc").join("coder").join("create").join("S00-base.kai");
        assert!(
            fs::symlink_metadata(&dest).expect("stat the destination").file_type().is_symlink(),
            "a refused overlay must not have replaced anything"
        );
    }

    #[test]
    fn a_symlinked_directory_inside_the_overlay_refuses_before_it_is_followed() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("variant");
        fs::create_dir_all(&overlay).expect("create the overlay");

        let escape_target = overlay_parent.path().join("escape");
        fs::create_dir_all(escape_target.join("create")).expect("create an escape target");
        fs::write(escape_target.join("create").join("S00-base.kai"), "# escaped\n")
            .expect("write a file the walk must never reach");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&escape_target, overlay.join("coder"))
            .expect("symlink a whole type directory out of the overlay");

        let error = install_rc_overlay(&state, &overlay)
            .expect_err("a symlinked directory must refuse, not be followed");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_regular_file_inside_the_overlay_refuses() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("variant");
        let coder_dir = overlay.join("coder").join("create");
        fs::create_dir_all(&coder_dir).expect("create the overlay's coder dir");

        let fifo = coder_dir.join("S00-base.kai");
        let c_path = std::ffi::CString::new(fifo.to_str().expect("utf-8 path")).expect("cstring");
        // SAFETY: mkfifo takes a NUL-terminated path and a mode; it creates a
        // FIFO at a path this test owns and touches nothing else.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        let error = install_rc_overlay(&state, &overlay)
            .expect_err("a FIFO is neither a file nor a directory and must refuse");
        assert!(error.to_string().contains("neither a regular file nor a directory"), "{error}");
    }

    #[test]
    fn a_typo_d_type_name_refuses_rather_than_creating_one() {
        let (_seed_parent, state, _lib_script) = seeded_rc_tree();
        let overlay_parent = tempfile::tempdir().expect("overlay parent");
        let overlay = overlay_parent.path().join("variant");
        let typo_dir = overlay.join("coderr").join("create");
        fs::create_dir_all(&typo_dir).expect("create the overlay's typo'd dir");
        fs::write(typo_dir.join("S00-base.kai"), "# typo'd type\n").expect("write the file");

        let error = install_rc_overlay(&state, &overlay)
            .expect_err("a directory absent from the seeded tree must refuse");
        assert!(error.to_string().contains("no seeded directory"), "{error}");

        let typo_in_seeded_tree = state.config_root().join("rc").join("coderr");
        assert!(!typo_in_seeded_tree.exists(), "a typo must never create a new type directory");
    }
}
