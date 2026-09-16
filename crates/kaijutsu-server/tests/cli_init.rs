//! CLI coverage for `kaijutsu-server init` (`docs/character.md`, "Bootstrap:
//! the person creates themself"): the real compiled binary against a
//! temporary `$HOME`, with no server running.

use std::path::{Path, PathBuf};
use std::process::Command;

use kaijutsu_kernel::kernel_db::{CharacterRow, KernelDb};
use kaijutsu_server::AuthDb;
use kaijutsu_types::PrincipalId;
use russh::keys::{Algorithm, PrivateKey};

fn server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kaijutsu-server")
}

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

    fn kernel_db_path(&self) -> PathBuf {
        self.path().join(".local/share/kaijutsu/kernel/kernel.db")
    }

    fn auth_db_path(&self) -> PathBuf {
        self.path().join(".local/share/kaijutsu/auth.db")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
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

    fn character(&self, name: &str) -> Option<CharacterRow> {
        KernelDb::open_read_only(self.kernel_db_path()).unwrap().get_character_by_name(name).unwrap()
    }

    fn key_owner(&self, fingerprint: &str) -> Option<PrincipalId> {
        AuthDb::open(self.auth_db_path()).unwrap().authenticate(fingerprint).unwrap()
    }
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// On a fresh home, `init` creates `kernel.db`, a live root character, and
/// binds the key to it.
#[test]
fn init_on_a_fresh_home_creates_a_root_and_binds_the_key() {
    let home = Home::new();
    let (key, fingerprint) = home.write_pubkey("laptop");

    let out = home.run(&["init", "--as", "amy", "--key", key.to_str().unwrap()]);
    assert!(out.status.success(), "init failed: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("amy") && stdout.contains(&fingerprint), "{stdout}");

    let amy = home.character("amy").expect("init creates the character");
    assert!(amy.root, "init creates a root character");
    assert!(amy.retired_at.is_none());
    assert_eq!(home.key_owner(&fingerprint), Some(amy.principal_id));
}

/// Running the same `init` twice changes nothing and succeeds.
#[test]
fn init_repeated_is_a_no_op() {
    let home = Home::new();
    let (key, fingerprint) = home.write_pubkey("laptop");
    let args = ["init", "--as", "amy", "--key", key.to_str().unwrap()];
    assert!(home.run(&args).status.success());
    let first = home.character("amy").unwrap();

    let again = home.run(&args);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(home.character("amy").unwrap().principal_id, first.principal_id);
    assert_eq!(home.key_owner(&fingerprint), Some(first.principal_id));
}

/// A kernel that already has a different live root refuses `init` and says
/// how to add another root instead.
#[test]
fn init_refuses_when_another_root_exists() {
    let home = Home::new();
    let (amy_key, _) = home.write_pubkey("amy");
    assert!(home.run(&["init", "--as", "amy", "--key", amy_key.to_str().unwrap()]).status.success());

    let (bob_key, bob_fingerprint) = home.write_pubkey("bob");
    let out = home.run(&["init", "--as", "bob", "--key", bob_key.to_str().unwrap()]);
    assert!(!out.status.success(), "a second init with another name must refuse");
    let message = stderr(&out);
    assert!(message.contains("amy"), "the refusal names the existing root: {message}");
    assert!(message.contains("kj character create bob --root"), "{message}");
    assert!(home.character("bob").is_none(), "a refused init creates nothing");
    assert_eq!(home.key_owner(&bob_fingerprint), None);
}

/// An existing live character becomes the root, keeping its principal id.
#[test]
fn init_makes_an_existing_character_a_root() {
    let home = Home::new();
    let amy = PrincipalId::new();
    KernelDb::open(home.kernel_db_path()).unwrap().insert_character(&CharacterRow {
        principal_id: amy,
        name: "amy".into(),
        created_at: 1,
        retired_at: None,
        handoff_ctx: None,
        root_ctx: None,
        root: false,
    }).unwrap();
    let (key, fingerprint) = home.write_pubkey("laptop");

    let out = home.run(&["init", "--as", "amy", "--key", key.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let sheet = home.character("amy").unwrap();
    assert_eq!(sheet.principal_id, amy);
    assert!(sheet.root);
    assert_eq!(home.key_owner(&fingerprint), Some(amy));
}

/// A key already bound to another character is never moved by `init`.
#[test]
fn init_refuses_a_key_bound_to_another_character() {
    let home = Home::new();
    let other = PrincipalId::new();
    KernelDb::open(home.kernel_db_path()).unwrap().insert_character(&CharacterRow {
        principal_id: other,
        name: "banto".into(),
        created_at: 1,
        retired_at: None,
        handoff_ctx: None,
        root_ctx: None,
        root: false,
    }).unwrap();
    let (key, fingerprint) = home.write_pubkey("laptop");
    assert!(home.run(&["add-key", key.to_str().unwrap(), "--as", "banto"]).status.success());

    let out = home.run(&["init", "--as", "amy", "--key", key.to_str().unwrap()]);
    assert!(!out.status.success(), "init must not move a bound key");
    let message = stderr(&out);
    assert!(message.contains("banto") && message.contains("--rebind"), "{message}");
    assert!(home.character("amy").is_none(), "a refused init creates nothing");
    assert_eq!(home.key_owner(&fingerprint), Some(other));
}

/// `init` requires both flags.
#[test]
fn init_requires_as_and_key() {
    let home = Home::new();
    let (key, _) = home.write_pubkey("laptop");
    for args in [vec!["init", "--as", "amy"], vec!["init", "--key", key.to_str().unwrap()]] {
        let out = home.run(&args);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(stderr(&out).contains("Usage: kaijutsu-server init --as <name> --key <pubkey-file>"), "{}", stderr(&out));
    }
}
