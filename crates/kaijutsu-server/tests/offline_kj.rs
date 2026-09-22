//! CLI coverage for `kaijutsu-server kj` (`docs/server-cli.md`): running a
//! `kj` verb against a stopped kernel — the real compiled binary against a
//! temporary `$HOME`, with no server running.

mod common;
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kaijutsu_server::offline::KernelLock;
use russh::keys::{Algorithm, PrivateKey};

fn server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kaijutsu-server")
}

/// A fresh `$HOME`, with no server ever started against it.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().unwrap() }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `create_shared_kernel`'s data directory under this `$HOME` —
    /// `kernel_data_dir()`'s default, hardcoded here the same way
    /// `cli_init.rs` and `cli_keyring.rs` already pin it.
    fn kernel_dir(&self) -> PathBuf {
        self.path().join(".local/share/kaijutsu/kernel")
    }

    fn kernel_db_path(&self) -> PathBuf {
        self.kernel_dir().join("kernel.db")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(server_bin())
            .args(args)
            .env("HOME", self.path())
            .env_remove("XDG_DATA_HOME")
            .output()
            .expect("run kaijutsu-server")
    }

    fn write_pubkey(&self, name: &str) -> (PathBuf, String) {
        let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let mut pubkey = key.public_key().clone();
        pubkey.set_comment(name);
        let path = self.path().join(format!("{name}.pub"));
        std::fs::write(&path, pubkey.to_openssh().unwrap()).unwrap();
        let fingerprint = pubkey.fingerprint(russh::keys::ssh_key::HashAlg::Sha256).to_string();
        (path, fingerprint)
    }

    /// `init` the root character, then disable the embedding service so a
    /// `kj` boot never reaches the network — the same fixture every other
    /// ephemeral test kernel in this crate uses
    /// (`ssh.rs::an_ephemeral_config_carries_no_network_scorer`).
    fn bootstrap(&self, root_name: &str) {
        let (key, _) = self.write_pubkey("laptop");
        let out = self.run(&["init", "--as", root_name, "--key", key.to_str().unwrap()]);
        assert!(out.status.success(), "init failed: {}", stderr(&out));
        support::disable_embeddings(&self.kernel_dir());
    }

    fn character_names(&self) -> Vec<String> {
        let db = kaijutsu_kernel::KernelDb::open_read_only(self.kernel_db_path()).unwrap();
        db.list_characters(true).unwrap().into_iter().map(|row| row.name).collect()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `kj -- character create bob` mints a live character from the sole root's
/// own console (no `--as` needed with one live root); plain `kj --
/// character list` then names it in its human table.
///
/// `kj --json -- character list` (the OFFLINE CLI's own `--json`, not the
/// leaf's) prints the result's `.data` instead of that table — here the
/// array of principal-id hex strings `character list` always carries
/// alongside its message (`kj/character.rs::character_list`), which is
/// valid JSON and is NOT the same text as the plain message.
#[test]
fn character_create_then_json_list_prints_data_not_the_message() {
    let home = Home::new();
    home.bootstrap("amy");

    let create = home.run(&["kj", "--", "character", "create", "bob"]);
    assert!(create.status.success(), "kj character create failed: {}", stderr(&create));
    assert!(stdout(&create).contains("bob"), "{}", stdout(&create));

    let table = home.run(&["kj", "--", "character", "list"]);
    assert!(table.status.success(), "kj character list failed: {}", stderr(&table));
    assert!(stdout(&table).contains("bob"), "the plain message names bob: {}", stdout(&table));
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout(&table).trim()).is_err(),
        "the plain message must not itself be JSON, or this test cannot tell --json apart: {}",
        stdout(&table)
    );

    let json = home.run(&["kj", "--json", "--", "character", "list"]);
    assert!(json.status.success(), "kj --json character list failed: {}", stderr(&json));
    let value: serde_json::Value = serde_json::from_str(stdout(&json).trim())
        .unwrap_or_else(|e| panic!("kj --json must print valid JSON: {e}: {}", stdout(&json)));
    let ids = value
        .as_array()
        .unwrap_or_else(|| panic!("character list's data must be an array: {value}"));
    // amy (the root) and bob — both live.
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.iter().all(|id| id.as_str().unwrap().chars().all(|c| c.is_ascii_hexdigit())), "{ids:?}");
    assert_ne!(stdout(&json), stdout(&table), "--json must print the data, not the plain message");
}

/// `kj -- context create lane --type coder --as bob` runs from the caller's
/// root context (the sole live root, joined with no `--context` needed) and
/// creates a context played by `bob`; `kj -- context info lane` then names
/// bob as its performer.
#[test]
fn context_create_and_info_name_the_performer() {
    let home = Home::new();
    home.bootstrap("amy");

    let bob = home.run(&["kj", "--", "character", "create", "bob"]);
    assert!(bob.status.success(), "{}", stderr(&bob));

    let create = home.run(&["kj", "--", "context", "create", "lane", "--type", "coder", "--as", "bob"]);
    assert!(create.status.success(), "kj context create failed: {}", stderr(&create));

    let info = home.run(&["kj", "--", "context", "info", "lane"]);
    assert!(info.status.success(), "kj context info failed: {}", stderr(&info));
    assert!(stdout(&info).contains("bob"), "context info must name the performer: {}", stdout(&info));
}

/// A `kernel.lock` held by another process (here: this test process, holding
/// it directly) makes `kj` refuse at once — no kernel boot, and no write to
/// `kernel.db` at all.
#[test]
fn a_held_lock_makes_kj_refuse_without_touching_the_database() {
    let home = Home::new();
    home.bootstrap("amy");
    let before = home.character_names();

    let _held = KernelLock::acquire(&home.kernel_dir()).expect("test process takes the lock first");

    let out = home.run(&["kj", "--", "character", "create", "carol"]);
    assert!(!out.status.success(), "kj must refuse while the lock is held");
    assert!(stderr(&out).contains("kernel.lock"), "the refusal should name the lock path: {}", stderr(&out));

    drop(_held);
    assert_eq!(home.character_names(), before, "a refused kj must not touch the database");
}

/// With more than one live root character, `kj` refuses without `--as` and
/// lists every root by name; `--as <name>` then picks one.
#[test]
fn as_is_required_when_several_roots_exist() {
    let home = Home::new();
    home.bootstrap("amy");

    let carol = home.run(&["kj", "--", "character", "create", "carol", "--root"]);
    assert!(carol.status.success(), "{}", stderr(&carol));

    let ambiguous = home.run(&["kj", "--", "character", "list"]);
    assert!(!ambiguous.status.success(), "kj must refuse with no --as when several roots exist");
    let message = stderr(&ambiguous);
    assert!(message.contains("amy") && message.contains("carol"), "{message}");
    assert!(message.contains("--as"), "{message}");

    let disambiguated = home.run(&["kj", "--as", "amy", "--", "character", "list"]);
    assert!(disambiguated.status.success(), "--as amy must resolve the caller: {}", stderr(&disambiguated));
}

/// `kj drive` returns once the turn is admitted (`kj/drive.rs`), but the
/// offline runner's `settle` must not reach `shutdown_runtime_worker` — which
/// hard-interrupts every admitted turn (`runtime/turn_request.rs`) — until
/// the turn itself has finished. A mock backend replays a canned response as
/// a well-formed text stream with no script directory needed, so the turn
/// completes on its own and leaves a done model block behind.
#[test]
fn drive_offline_holds_the_command_until_the_turn_completes() {
    let home = Home::new();
    home.bootstrap("amy");
    common::seed_mock_backend_with_model(&home.kernel_dir(), "mock-model");

    let bob = home.run(&["kj", "--", "character", "create", "bob"]);
    assert!(bob.status.success(), "{}", stderr(&bob));

    let create = home.run(&["kj", "--", "context", "create", "lane", "--type", "coder", "--as", "bob"]);
    assert!(create.status.success(), "kj context create failed: {}", stderr(&create));

    let drive = home.run(&["kj", "--", "drive", "lane", "--prompt", "hi"]);
    assert!(drive.status.success(), "kj drive failed: {}", stderr(&drive));

    let list = home.run(&["kj", "--", "block", "list", "-c", "lane"]);
    assert!(list.status.success(), "kj block list failed: {}", stderr(&list));
    let out = stdout(&list);
    assert!(
        out.contains("model/") && out.contains("[done]"),
        "kj drive must hold the command until the turn ends, leaving a done model block: {out}"
    );
}

/// `kj ledger allow|deny` would record a durable answer with no delivery
/// worker running to act on it offline, and the next serving boot retires
/// the ask unexecuted (`approval_resume.rs`, `recover_unpublished_pairs`).
/// Both are refused before the kernel ever boots — on a fresh, unbootstrapped
/// `$HOME` so a database appearing at all would be the failure.
#[test]
fn ledger_allow_and_deny_are_refused_before_boot() {
    let home = Home::new();

    for verb in ["allow", "deny"] {
        let out = home.run(&["kj", "--", "ledger", verb, "01a0-does-not-exist"]);
        assert!(!out.status.success(), "kj ledger {verb} must refuse offline");
        let message = stderr(&out);
        assert!(
            message.contains("running kernel") && message.contains("SSH"),
            "kj ledger {verb} must say asks are answered on the running kernel over SSH: {message}"
        );
    }
    assert!(
        !home.kernel_db_path().exists(),
        "a refused ledger answer must never open kernel.db: {}",
        home.kernel_db_path().display()
    );
}

/// `--as <name>` must refuse a retired root by name: `character retire`
/// stamps `retired_at`, and a retired character's own turn is over, not just
/// its context work.
#[test]
fn as_refuses_a_retired_root_by_name() {
    let home = Home::new();
    home.bootstrap("amy");

    let carol = home.run(&["kj", "--", "character", "create", "carol", "--root"]);
    assert!(carol.status.success(), "{}", stderr(&carol));

    let retire = home.run(&["kj", "--as", "amy", "--", "character", "retire", "carol", "--confirm"]);
    assert!(retire.status.success(), "kj character retire failed: {}", stderr(&retire));

    let out = home.run(&["kj", "--as", "carol", "--", "character", "list"]);
    assert!(!out.status.success(), "--as must refuse a retired root");
    assert!(stderr(&out).contains("retired"), "{}", stderr(&out));
}

/// `--as <name>` must refuse a name that resolves to a live but non-root
/// character, the same way it refuses one that does not exist.
#[test]
fn as_refuses_a_non_root_character() {
    let home = Home::new();
    home.bootstrap("amy");

    let bob = home.run(&["kj", "--", "character", "create", "bob"]);
    assert!(bob.status.success(), "{}", stderr(&bob));

    let out = home.run(&["kj", "--as", "bob", "--", "character", "list"]);
    assert!(!out.status.success(), "--as must refuse a non-root character");
    assert!(stderr(&out).contains("not a root character"), "{}", stderr(&out));
}

/// A second `kj` invocation started right after the first exits must succeed:
/// the lock is released and the WAL is checkpointed before the process
/// exits, not sometime later.
#[test]
fn a_second_kj_invocation_right_after_the_first_succeeds() {
    let home = Home::new();
    home.bootstrap("amy");

    let first = home.run(&["kj", "--", "character", "create", "bob"]);
    assert!(first.status.success(), "first kj invocation failed: {}", stderr(&first));

    let second = home.run(&["kj", "--", "character", "list"]);
    assert!(
        second.status.success(),
        "a second kj right after the first must succeed (lock/WAL not released?): {}",
        stderr(&second)
    );
    assert!(stdout(&second).contains("bob"), "{}", stdout(&second));
}

/// A Destroy verb dispatched with no `--confirm` hits `kj`'s own confirmation
/// gate (`KjResult::Latch`), not the approval ledger, and exits 2 — the same
/// code the kaish `kj` builtin uses for the same case.
#[test]
fn a_destroy_verb_without_confirm_exits_2() {
    let home = Home::new();
    home.bootstrap("amy");

    let create = home.run(&["kj", "--", "context", "create", "lane", "--type", "coder"]);
    assert!(create.status.success(), "kj context create failed: {}", stderr(&create));

    let archive = home.run(&["kj", "--", "context", "archive", "lane"]);
    assert_eq!(
        archive.status.code(),
        Some(2),
        "a Destroy verb with no --confirm must exit 2: {}",
        stderr(&archive)
    );
    assert!(stderr(&archive).contains("--confirm"), "{}", stderr(&archive));
}
