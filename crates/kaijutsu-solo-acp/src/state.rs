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
pub fn prepare_rows(
    state: &SoloState,
    root_character: &str,
    performer: &str,
    key: &PrivateKey,
    choice: &ModelChoice,
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
}
