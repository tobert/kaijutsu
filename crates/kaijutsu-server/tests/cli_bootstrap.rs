//! CLI coverage for `kaijutsu-server bootstrap`: the real compiled binary
//! against a temporary `$HOME`, finding what it can there.

use std::path::Path;
use std::process::{Command, Output};

use russh::keys::{Algorithm, PrivateKey};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kaijutsu-server"))
        .args(args)
        .env("HOME", home)
        .env("USER", "amy")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .output()
        .expect("run kaijutsu-server")
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// A home with the usual public key and a DeepSeek key file: bootstrap
/// finds both, with no flags, and a second run adds nothing.
#[test]
fn bootstrap_finds_the_key_and_the_provider_in_home() {
    let home = tempfile::tempdir().unwrap();
    let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/id_ed25519.pub"), key.public_key().to_openssh().unwrap()).unwrap();
    std::fs::write(home.path().join(".deepseek-key"), "sk-test\n").unwrap();

    let first = run(home.path(), &["bootstrap"]);
    assert!(first.status.success(), "bootstrap failed: {}", text(&first));
    let first = text(&first);
    for line in [
        "ran: kj backend set deepseek --kind deepseek",
        "ran: kj backend model set deepseek deepseek-flash --context-window 1000000 --max-tokens 65536 --effort high",
        "ran: kj character create root",
        "ran: kj character slot set root console --backend deepseek --model deepseek-flash --max-tokens 16384",
        "ran: kj character slot set root distill --backend deepseek --model deepseek-flash --max-tokens 16384",
        "ran: kj context create ROOT --type console --as root",
    ] {
        assert!(first.contains(line), "missing {line:?} in:\n{first}");
    }

    let second = run(home.path(), &["bootstrap"]);
    assert!(second.status.success(), "second bootstrap failed: {}", text(&second));
    assert!(!text(&second).contains("ran:"), "a second bootstrap runs nothing:\n{}", text(&second));
}

/// `--model` with no key found refuses rather than drop the model.
#[test]
fn a_model_with_no_provider_found_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/id_ed25519.pub"), key.public_key().to_openssh().unwrap()).unwrap();

    let out = run(home.path(), &["bootstrap", "--model", "deepseek-flash"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("--backend"), "{}", text(&out));
}

/// Without a public key, bootstrap refuses and names the flag.
#[test]
fn bootstrap_without_a_public_key_names_the_flag() {
    let home = tempfile::tempdir().unwrap();
    let out = run(home.path(), &["bootstrap"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("--key"), "{}", text(&out));
}
