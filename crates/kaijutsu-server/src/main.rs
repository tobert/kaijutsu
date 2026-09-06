//! Kaijutsu server binary
//!
//! SSH + Cap'n Proto RPC server for kaijutsu.
//!
//! ## Usage
//!
//! ```bash
//! # Run the server (default)
//! kaijutsu-server [port]
//!
//! # Key management — auth.db is a keyring: it binds a fingerprint to a
//! # principal id and carries no name (docs/character.md, "auth.db is a
//! # keyring"). `kj character create <name>` mints the principal id first.
//! kaijutsu-server add-key <pubkey-file> --as <character> [--rebind]
//! kaijutsu-server list-keys
//!
//! # Lockout recovery — reads kernel.db read-only, works with the service
//! # stopped, needs no connection or authentication.
//! kaijutsu-server list-characters
//!
//! # One-time upgrade of a pre-melt auth.db: harvest its usernames into
//! # kernel.db as characters, then drop the columns that held them. Run
//! # once, deliberately, with the service stopped. No-op if already melted.
//! kaijutsu-server migrate-keyring
//!
//! # rc scripts (no running kernel needed)
//! kaijutsu-server rc reseed [--force] [--dir <path>]
//!
//! `rc reseed` installs anything absent and NAMES anything present that
//! differs from its embedded default, leaving it alone; `--force` overwrites
//! those instead.
//! ```

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_server::config_mounts::ConfigMounts;
use kaijutsu_server::constants::DEFAULT_SSH_PORT;
use kaijutsu_server::rpc::kernel_data_dir;
use kaijutsu_server::{AuthDb, SshServer, SshServerConfig};
use russh::keys::ssh_key::{self, HashAlg};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn print_usage() {
    eprintln!(
        r#"kaijutsu-server - SSH + Cap'n Proto server for kaijutsu

USAGE:
    kaijutsu-server [OPTIONS] [COMMAND]

COMMANDS:
    (default)                       Run the SSH server
    add-key <file> --as <name>      Bind a public key to an existing character.
                                    Refuses an already-bound fingerprint; pass
                                    --rebind to move it instead.
    list-keys                       List every bound fingerprint.
    list-characters                 List characters from kernel.db, read-only.
                                    Works with the service stopped — the
                                    lockout-recovery path.
    migrate-keyring                 One-time upgrade of a pre-melt auth.db:
                                    harvest its usernames into kernel.db as
                                    characters, then drop the columns that
                                    held them. No-op if already melted.
    rc reseed [--force] [--dir D]   Install embedded rc scripts into the rc tree.
                                    Names any script that differs from its
                                    default and leaves it alone; --force
                                    overwrites those.

OPTIONS:
    --config-root <DIR>           Where the /config trees live
                                  (default: ~/.config/kaijutsu/config).
                                  Each tree is a subdirectory unless declared.
    --mount <TREE>=<DIR>          Point one tree elsewhere, e.g.
                                  --mount /config/rc=./assets/defaults/rc.
                                  Beats <config-root>/mounts.toml.
    --port <PORT>                 SSH port (default: {port})
    --as <NAME>                   add-key: the character to bind the key to.
    --rebind                      add-key: move an already-bound key instead
                                  of refusing.
    --help, -h                    Show this help

EXAMPLES:
    kaijutsu-server                                    # Run server on port {port}
    kaijutsu-server --port 2222                        # Run server on port 2222
    kaijutsu-server add-key ~/.ssh/id_ed25519.pub --as hajime
    kaijutsu-server add-key ~/.ssh/id_ed25519.pub --as amy --rebind
    kaijutsu-server list-keys
    kaijutsu-server list-characters
    kaijutsu-server migrate-keyring                    # one-time, pre-melt auth.db only
    kaijutsu-server rc reseed                          # install anything missing, name what differs
    kaijutsu-server rc reseed --force                  # also overwrite what differs
    kaijutsu-server rc reseed --dir ./rc                # seed a directory of your choosing

DATABASES:
    Keys are stored in:       {auth_db_path}
    Characters live in:       {kernel_db_path}
"#,
        port = DEFAULT_SSH_PORT,
        auth_db_path = AuthDb::default_path().display(),
        kernel_db_path = default_kernel_db_path().display(),
    );
}

/// The default `kernel.db` path — same default the running server's own
/// bootstrap uses (`kaijutsu_server::rpc::kernel_data_dir`), so `add-key
/// --as` and `list-characters` resolve a name against the exact file a live
/// server would.
fn default_kernel_db_path() -> PathBuf {
    kernel_data_dir().join("kernel.db")
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr));

    let _otel_guard = if kaijutsu_telemetry::otel_enabled() {
        let (otel_layer, guard) = kaijutsu_telemetry::otel_layer("kaijutsu-server");
        registry.with(otel_layer).init();
        Some(guard)
    } else {
        registry.init();
        None
    };

    let args: Vec<String> = env::args().collect();

    // Parse command
    if args.len() < 2 {
        return run_server(DEFAULT_SSH_PORT, ServerPaths::default()).await;
    }

    // Server-shaped flags may appear in any order and are consumed before the
    // subcommand match, so `--mount X=Y --port 2222` and `--port 2222 --mount
    // X=Y` mean the same thing.
    let (args, server_paths) = match ServerPaths::take_from(&args) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if args.len() < 2 {
        return run_server(DEFAULT_SSH_PORT, server_paths).await;
    }

    match args[1].as_str() {
        "--help" | "-h" => {
            print_usage();
            ExitCode::SUCCESS
        }
        "--port" => {
            let port = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SSH_PORT);
            run_server(port, server_paths).await
        }
        "add-key" => cmd_add_key(&args[2..]),
        "list-keys" => cmd_list_keys(),
        "list-characters" => cmd_list_characters(),
        "migrate-keyring" => cmd_migrate_keyring(),
        "rc" => cmd_rc(&args[2..]),
        arg => {
            // Try parsing as port number for backwards compatibility
            if let Ok(port) = arg.parse::<u16>() {
                return run_server(port, server_paths).await;
            }
            eprintln!("Unknown command: {}", arg);
            print_usage();
            ExitCode::FAILURE
        }
    }
}

/// The `--config-root` / `--mount` half of the command line: where every
/// `/config` tree comes from (`docs/config-namespace.md`).
#[derive(Debug, Default)]
struct ServerPaths {
    root: Option<PathBuf>,
    mounts: Vec<String>,
}

impl ServerPaths {
    /// Consume the path flags from `argv`, returning what is left plus what
    /// was found. Flags are removed so the subcommand match below sees only
    /// its own arguments.
    fn take_from(args: &[String]) -> Result<(Vec<String>, Self), String> {
        let mut out = Vec::with_capacity(args.len());
        let mut me = Self::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--config-root" => {
                    let v = args
                        .get(i + 1)
                        .ok_or("--config-root needs a directory")?;
                    me.root = Some(PathBuf::from(v));
                    i += 2;
                }
                "--mount" => {
                    let v = args.get(i + 1).ok_or("--mount needs <tree>=<dir>")?;
                    me.mounts.push(v.clone());
                    i += 2;
                }
                other => {
                    out.push(other.to_string());
                    i += 1;
                }
            }
        }
        Ok((out, me))
    }

    /// Resolve to a registry: the root, then `mounts.toml` inside it, then the
    /// `--mount` flags, each beating the last.
    fn into_mounts(self) -> Result<ConfigMounts, String> {
        let mut mounts =
            ConfigMounts::new(self.root.unwrap_or_else(ConfigMounts::default_root));
        mounts.load_declarations()?;
        for arg in &self.mounts {
            mounts.set_from_arg(arg)?;
        }
        Ok(mounts)
    }
}

async fn run_server(port: u16, paths: ServerPaths) -> ExitCode {
    // Resolve the config mounts BEFORE announcing a start. A bad declaration
    // is a refusal to boot, and saying "Starting..." first would report a
    // server that came up and died rather than one that never began.
    let mounts = match paths.into_mounts() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("config mounts: {e}");
            return ExitCode::FAILURE;
        }
    };

    tracing::info!("Starting kaijutsu server on SSH port {}...", port);
    let mut config = SshServerConfig::production(port);
    config.config_mounts = mounts;
    let server = SshServer::new(config);

    if let Err(e) = server.run().await {
        tracing::error!("Server error: {}", e);
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// `rc reseed` — write the embedded rc scripts into the rc tree.
///
/// This runs against a directory, not a kernel: no server, no context, no
/// capability, and nothing to approve. That is the point — it is what you run
/// before starting the kernel, and it cannot be blocked by the kernel it is
/// about to configure.
fn cmd_rc(args: &[String]) -> ExitCode {
    let Some(sub) = args.first().map(String::as_str) else {
        eprintln!("Usage: kaijutsu-server rc reseed [--force] [--dir <path>]");
        return ExitCode::FAILURE;
    };
    if sub != "reseed" {
        eprintln!("Unknown rc subcommand: {sub}");
        eprintln!("Usage: kaijutsu-server rc reseed [--force] [--dir <path>]");
        return ExitCode::FAILURE;
    }

    let mut force = false;
    let mut dir: Option<PathBuf> = None;
    let mut rest = args[1..].iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--force" | "-f" => force = true,
            "--dir" => match rest.next() {
                Some(d) => dir = Some(PathBuf::from(d)),
                None => {
                    eprintln!("--dir needs a path");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("Unknown option: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let root = dir.unwrap_or_else(kaijutsu_server::ssh::default_rc_dir);
    match kaijutsu_kernel::seed_scripts::reseed_rc_files(&root, force) {
        Ok(r) => {
            println!(
                "{}: {} written, {} replaced, {} unchanged",
                root.display(),
                r.written,
                r.replaced,
                r.unchanged
            );
            // Name what was left alone, never just count it. A tree pointed
            // at a checkout reports every entry here, which is the signal
            // that the root is not where you thought it was.
            if !r.diverged.is_empty() {
                println!(
                    "\n{} file(s) differ from their defaults and were left alone:",
                    r.diverged.len()
                );
                for path in &r.diverged {
                    println!("  {path}");
                }
                println!("\nPass --force to overwrite them with the embedded defaults.");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rc reseed into {} failed: {e}", root.display());
            ExitCode::FAILURE
        }
    }
}

/// Parsed `add-key` arguments.
struct AddKeyArgs {
    key_file: String,
    character: String,
    rebind: bool,
}

fn parse_add_key_args(args: &[String]) -> Result<AddKeyArgs, String> {
    if args.is_empty() {
        return Err("Usage: kaijutsu-server add-key <pubkey-file> --as <character> [--rebind]".to_string());
    }
    let key_file = args[0].clone();
    let mut character: Option<String> = None;
    let mut rebind = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--as" => {
                if i + 1 < args.len() {
                    character = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err("--as requires a character name".to_string());
                }
            }
            "--rebind" => {
                rebind = true;
                i += 1;
            }
            other => return Err(format!("Unknown option: {other}")),
        }
    }

    let character = character
        .ok_or_else(|| "--as <character> is required — `kaijutsu-server list-characters` to see who exists".to_string())?;
    Ok(AddKeyArgs { key_file, character, rebind })
}

/// Bind a public key to an existing character. Never mints: the character's
/// principal id must already exist in `kernel.db` (`kj character create
/// <name>`), and this only writes `auth.db`
/// (`docs/character.md`, "Adding a key binds; it never mints").
fn cmd_add_key(args: &[String]) -> ExitCode {
    let parsed = match parse_add_key_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // Expand path (handle ~)
    let key_path: PathBuf = shellexpand::tilde(&parsed.key_file).as_ref().into();

    // Read and parse the key
    let key_data = match std::fs::read_to_string(&key_path) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Failed to read {}: {}", key_path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let key = match ssh_key::PublicKey::from_openssh(key_data.trim()) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("Failed to parse public key: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let comment = extract_comment(key_data.trim());

    // Resolve the character's name to a principal id — a genuine read-only
    // connection, so this never contends with a live server's write
    // connection and works whether or not the service is running.
    let kernel_db_path = default_kernel_db_path();
    let kdb = match KernelDb::open_read_only(&kernel_db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!(
                "Failed to open {} read-only: {e}\n\
                 (start the server once first — it seeds the bootstrap character)",
                kernel_db_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let character = match kdb.get_character_by_name(&parsed.character) {
        Ok(Some(row)) => row,
        Ok(None) => {
            eprintln!(
                "No character named '{}' — `kaijutsu-server list-characters` to see who exists",
                parsed.character
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("Failed to read {}: {e}", kernel_db_path.display());
            return ExitCode::FAILURE;
        }
    };

    // Open auth.db for the write.
    let auth_db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let existing = match auth_db.get_key(&fingerprint) {
        Ok(existing) => existing,
        Err(e) => {
            eprintln!("Database error: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if let Some(existing) = existing {
        if !parsed.rebind {
            // Never a silent move — name the current binding and point at
            // the escape hatch (`docs/character.md`, "`add-key` never
            // rebinds silently").
            let current_name = kdb.get_character(existing.principal_id);
            let current_name = match current_name {
                Ok(Some(row)) => row.name,
                _ => existing.principal_id.short(),
            };
            eprintln!(
                "key {fingerprint} is bound to {current_name}; move it with --rebind"
            );
            return ExitCode::FAILURE;
        }
        match auth_db.rebind_key(character.principal_id, &key, comment.as_deref()) {
            Ok(fp) => {
                println!(
                    "Moved key {fp} to {} ({})",
                    character.name,
                    character.principal_id.short()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Failed to rebind key: {}", e);
                ExitCode::FAILURE
            }
        }
    } else {
        match auth_db.add_key(character.principal_id, &key, comment.as_deref()) {
            Ok(fp) => {
                println!(
                    "Bound key {fp} to {} ({})",
                    character.name,
                    character.principal_id.short()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("Failed to add key: {}", e);
                ExitCode::FAILURE
            }
        }
    }
}

/// List every bound fingerprint. Resolves each principal to its character's
/// name when `kernel.db` is reachable; falls back to the id's short form
/// otherwise (a stopped service, or a wiped kernel — `docs/character.md`,
/// "A kernel wipe orphans every binding").
fn cmd_list_keys() -> ExitCode {
    let auth_db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let keys = match auth_db.list_all_keys() {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("Failed to list keys: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if keys.is_empty() {
        println!(
            "No keys found. Add one with: kaijutsu-server add-key <pubkey> --as <character>"
        );
        return ExitCode::SUCCESS;
    }

    let kdb = KernelDb::open_read_only(default_kernel_db_path()).ok();
    let name_for = |id: kaijutsu_types::PrincipalId| -> String {
        kdb.as_ref()
            .and_then(|db| db.get_character(id).ok().flatten())
            .map(|row| row.name)
            .unwrap_or_else(|| id.short())
    };

    println!(
        "{:<16} {:<12} {:<48} COMMENT",
        "CHARACTER", "TYPE", "FINGERPRINT"
    );
    println!("{}", "-".repeat(90));

    for key in keys {
        let comment = key.comment.as_deref().unwrap_or("");
        println!(
            "{:<16} {:<12} {:<48} {}",
            name_for(key.principal_id),
            key.key_type,
            key.fingerprint,
            comment
        );
    }

    ExitCode::SUCCESS
}

/// List characters from `kernel.db`, read-only. The lockout-recovery path:
/// reaches no network, needs no authentication, and works with the service
/// stopped (`docs/character.md`, "Lockout recovery is why the CLI lists
/// characters").
fn cmd_list_characters() -> ExitCode {
    let path = default_kernel_db_path();
    let db = match KernelDb::open_read_only(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!(
                "Failed to open {} read-only: {e}\n\
                 (start the server once first — it seeds the bootstrap character)",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let characters = match db.list_characters(true) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to list characters: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if characters.is_empty() {
        println!("No characters found — this kernel.db was never bootstrapped.");
        return ExitCode::SUCCESS;
    }

    println!("{:<20} {:<34} STATUS", "NAME", "PRINCIPAL ID");
    println!("{}", "-".repeat(64));
    for c in characters {
        let status = if c.retired_at.is_some() { "retired" } else { "live" };
        println!("{:<20} {:<34} {}", c.name, c.principal_id.to_hex(), status);
    }

    ExitCode::SUCCESS
}

/// One-time upgrade of a pre-melt `auth.db`: harvest its usernames into
/// `kernel.db` as characters, then drop the columns that held them
/// (`docs/character.md`, "`auth.db` is a keyring";
/// `kaijutsu_server::migrate_keyring`). Opens both databases read-write at
/// their default paths — run this with the service stopped. A no-op,
/// reported as such, when `auth.db` has already been melted.
fn cmd_migrate_keyring() -> ExitCode {
    let auth_db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };
    let kernel_db_path = default_kernel_db_path();
    let mut kernel_db = match KernelDb::open(&kernel_db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open {}: {e}", kernel_db_path.display());
            return ExitCode::FAILURE;
        }
    };

    match kaijutsu_server::migrate_keyring::migrate_legacy_names(&auth_db, &mut kernel_db) {
        Ok(0) => {
            println!("Nothing to migrate — auth.db has already been melted.");
            ExitCode::SUCCESS
        }
        Ok(n) => {
            println!("Migrated {n} principal(s) into kernel.db characters. auth.db's legacy columns are dropped.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Migration failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Extract comment from an OpenSSH public key line
fn extract_comment(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() >= 3 {
        Some(parts[2].to_string())
    } else {
        None
    }
}
