//! CLI-level coverage for the keyring melt (`docs/character.md`, "`auth.db`
//! is a keyring"): `add-key --as`/`--rebind`, `list-keys`, and
//! `list-characters` as an operator actually runs them — the real compiled
//! binary, a temporary `$HOME`, no network and no live server.
//!
//! `kernel.db` is seeded directly with `KernelDb`, the way a server's own
//! bootstrap would leave it (a character or two already minted) — these
//! tests are about the CLI's behavior against an existing keyring, not
//! about server startup.

use std::path::{Path, PathBuf};
use std::process::Command;

use kaijutsu_kernel::kernel_db::{CharacterRow, KernelDb};
use kaijutsu_types::PrincipalId;
use russh::keys::{Algorithm, PrivateKey};

fn server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kaijutsu-server")
}

/// A fresh `$HOME` with `kernel.db` seeded at the exact path
/// `kernel_data_dir()` resolves to, carrying one named character.
struct Fixture {
    home: tempfile::TempDir,
    character: PrincipalId,
}

impl Fixture {
    fn new(character_name: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let kernel_db_path = home
            .path()
            .join(".local/share/kaijutsu/kernel/kernel.db");
        let db = KernelDb::open(&kernel_db_path).unwrap();
        let character = PrincipalId::new();
        db.insert_character(&CharacterRow {
            principal_id: character,
            name: character_name.to_string(),
            created_at: 1,
            retired_at: None,
                handoff_ctx: None,
        })
        .unwrap();
        Self { home, character }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(server_bin())
            .args(args)
            .env("HOME", self.home_path())
            .output()
            .expect("run kaijutsu-server")
    }

    fn write_pubkey(&self, name: &str) -> (PathBuf, String) {
        let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        let mut pubkey = key.public_key().clone();
        pubkey.set_comment(name);
        let path = self.home_path().join(format!("{name}.pub"));
        std::fs::write(&path, pubkey.to_openssh().unwrap()).unwrap();
        let fingerprint = pubkey
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
            .to_string();
        (path, fingerprint)
    }
}

/// `add-key --as <character>` binds a key to an existing character — the
/// only way in after the melt. `list-keys` then shows the character's name
/// against the fingerprint, and `list-characters` still shows exactly the
/// one seeded character.
#[test]
fn add_key_binds_and_list_commands_report_it() {
    let fx = Fixture::new("hajime");
    let (pubkey_path, fingerprint) = fx.write_pubkey("laptop");

    let out = fx.run(&["add-key", pubkey_path.to_str().unwrap(), "--as", "hajime"]);
    assert!(out.status.success(), "add-key failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hajime"), "add-key output should name the character: {stdout}");
    assert!(stdout.contains(&fingerprint), "add-key output should name the fingerprint: {stdout}");

    let list_keys = fx.run(&["list-keys"]);
    assert!(list_keys.status.success());
    let stdout = String::from_utf8_lossy(&list_keys.stdout);
    assert!(stdout.contains("hajime"), "list-keys must resolve the character's name: {stdout}");
    assert!(stdout.contains(&fingerprint));

    let list_chars = fx.run(&["list-characters"]);
    assert!(list_chars.status.success());
    let stdout = String::from_utf8_lossy(&list_chars.stdout);
    assert!(stdout.contains("hajime"));
    assert!(stdout.contains("live"), "a freshly seeded character is live, not retired");
}

/// `add-key` on an unknown character name fails loudly and points at
/// `list-characters` — never silently mints one.
#[test]
fn add_key_unknown_character_refuses() {
    let fx = Fixture::new("hajime");
    let (pubkey_path, _fp) = fx.write_pubkey("laptop");

    let out = fx.run(&["add-key", pubkey_path.to_str().unwrap(), "--as", "nobody"]);
    assert!(!out.status.success(), "add-key must refuse an unknown character");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nobody"), "the refusal should name the character that wasn't found: {stderr}");
    assert!(stderr.contains("list-characters"), "the refusal should point at the recovery command: {stderr}");
}

/// Re-adding an already-bound fingerprint refuses and names the CURRENT
/// binding — never a silent move. `--rebind` performs the move, and after
/// it the key authenticates to the new character, not the old one.
#[test]
fn re_adding_a_bound_key_refuses_and_names_the_binding_then_rebind_moves_it() {
    let fx = Fixture::new("hajime");
    let kernel_db_path = fx.home_path().join(".local/share/kaijutsu/kernel/kernel.db");
    let db = KernelDb::open(&kernel_db_path).unwrap();
    let amy = PrincipalId::new();
    db.insert_character(&CharacterRow {
        principal_id: amy,
        name: "amy".to_string(),
        created_at: 2,
        retired_at: None,
                handoff_ctx: None,
    })
    .unwrap();

    let (pubkey_path, fingerprint) = fx.write_pubkey("shared");
    let first = fx.run(&["add-key", pubkey_path.to_str().unwrap(), "--as", "hajime"]);
    assert!(first.status.success());

    // Re-adding without --rebind refuses and names "hajime", the current
    // binding — not a silent UPDATE.
    let refused = fx.run(&["add-key", pubkey_path.to_str().unwrap(), "--as", "amy"]);
    assert!(!refused.status.success(), "a bound fingerprint must refuse without --rebind");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains(&fingerprint), "the refusal should name the fingerprint: {stderr}");
    assert!(stderr.contains("hajime"), "the refusal should name the CURRENT binding: {stderr}");
    assert!(stderr.contains("--rebind"), "the refusal should point at the escape hatch: {stderr}");

    // --rebind performs the move.
    let rebound = fx.run(&["add-key", pubkey_path.to_str().unwrap(), "--as", "amy", "--rebind"]);
    assert!(rebound.status.success(), "rebind failed: {}", String::from_utf8_lossy(&rebound.stderr));
    let stdout = String::from_utf8_lossy(&rebound.stdout);
    assert!(stdout.contains("amy"), "rebind output should name the new character: {stdout}");

    let list_keys = fx.run(&["list-keys"]);
    let stdout = String::from_utf8_lossy(&list_keys.stdout);
    assert!(stdout.contains("amy"), "the key must now be listed under amy: {stdout}");
    assert!(!stdout.lines().any(|l| l.starts_with("hajime")), "and no longer under hajime: {stdout}");

    let _ = amy; // silence unused warning if the assertions above change
}

/// `list-characters` works with no server running at all, reading
/// `kernel.db` read-only — the lockout-recovery path.
#[test]
fn list_characters_works_with_no_server_running() {
    let fx = Fixture::new("hajime");
    let out = fx.run(&["list-characters"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hajime"));
    let _ = fx.character;
}

/// A `$HOME` where the server has never run at all has no `kernel.db` —
/// `list-characters` must fail loudly and explain why, never print an empty
/// table that could be mistaken for a legitimately bootstrapped, characterless
/// kernel.
#[test]
fn no_bootstrap_yet_fails_loudly_not_silently() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(server_bin())
        .args(["list-characters"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    // No kernel.db exists yet at all — this must fail loudly, not print an
    // empty table that looks like a legitimately-bootstrapped, characterless
    // kernel.
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("kernel.db") || stderr.contains("start the server"),
        "should explain that the server has never bootstrapped this HOME: {stderr}"
    );
}
