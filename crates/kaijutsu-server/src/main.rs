//! Kaijutsu server binary
//!
//! SSH + Cap'n Proto RPC server for kaijutsu. `kaijutsu-server --help` and
//! `kaijutsu-server kj --help` are the published reference; `docs/server-cli.md`
//! explains the offline `kj` runner in full.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_kernel::KernelMode;
use kaijutsu_server::config_mounts::ConfigMounts;
use kaijutsu_server::constants::DEFAULT_SSH_PORT;
use kaijutsu_server::offline::{KjRunArgs, run_kj};
use kaijutsu_server::rpc::kernel_data_dir;
use kaijutsu_server::{AuthDb, SshServer, SshServerConfig};
use russh::keys::ssh_key::{self, HashAlg};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(
    name = "kaijutsu-server",
    about = "SSH + Cap'n Proto server for kaijutsu",
    long_about = "With no subcommand, runs the SSH server. It refuses to start until \
        `bootstrap` or `init` has created a root character.",
    after_help = "EXAMPLES:\n    \
        kaijutsu-server                                              run on the default port\n    \
        kaijutsu-server --port 2222                                  run on port 2222\n    \
        kaijutsu-server bootstrap                                    before the first start\n    \
        kaijutsu-server init --as amy --key ~/.ssh/id_ed25519.pub    the root alone, no model\n    \
        kaijutsu-server add-key ~/.ssh/id_ed25519.pub --as amy --rebind\n    \
        kaijutsu-server list-keys\n    \
        kaijutsu-server list-characters\n    \
        kaijutsu-server rc reseed                                    install anything missing\n    \
        kaijutsu-server kj -- character create bob --root\n    \
        kaijutsu-server kj --as amy -- context create banto --type director --as banto\n\n\
        DATABASES:\n    \
        Keys live in <data-home>/kaijutsu/auth.db\n    \
        Characters live in <data-home>/kaijutsu/kernel/kernel.db\n    \
        <data-home> is $XDG_DATA_HOME, or ~/.local/share when unset."
)]
struct Cli {
    #[command(flatten)]
    paths: PathArgs,
    /// Serve a headless kernel: one that runs with little input from people,
    /// as under a harness. A coder's turn runs until it calls `done`, even
    /// when a person prompted it. Only the serving default reads this.
    #[arg(long)]
    headless: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Flags every subcommand accepts, but only the serving default, `rc reseed`,
/// `kj`, and `bootstrap` actually read: where the `/config` trees and read-write
/// workspaces come from, and the port the serving default binds. May appear
/// before or after a subcommand name. The direct-database commands (`init`,
/// `add-key`, `list-keys`, `list-characters`, `migrate-keyring`) always read
/// the default XDG paths and ignore these.
#[derive(Args)]
struct PathArgs {
    /// Where the /config trees live. Each tree is a subdirectory unless
    /// pointed elsewhere by --mount or <config-root>/mounts.toml. Default:
    /// $XDG_CONFIG_HOME/kaijutsu/config, or ~/.config/kaijutsu/config when
    /// XDG_CONFIG_HOME is unset.
    #[arg(long, global = true, value_name = "DIR")]
    config_root: Option<PathBuf>,

    /// Point one /config tree at a host directory, e.g.
    /// --mount /config/rc=./assets/defaults/rc. Repeatable. Beats
    /// <config-root>/mounts.toml for that tree.
    #[arg(long, global = true, value_name = "TREE=DIR")]
    mount: Vec<String>,

    /// Mount a host directory read-write at the same path in the kernel, so
    /// file tools may write there. Repeatable. This is a workspace, not a
    /// /config tree.
    #[arg(long, global = true, value_name = "DIR")]
    rw_mount: Vec<PathBuf>,

    /// SSH port for the serving default.
    #[arg(long, global = true, default_value_t = DEFAULT_SSH_PORT)]
    port: u16,
}

#[derive(Subcommand)]
enum Command {
    /// Set up a kernel so its ROOT console can answer: the root character
    /// and its key, a backend for each provider key found, the default
    /// model, ROOT's performer, and ROOT. Run with the service stopped. A
    /// second run adds only what is missing.
    Bootstrap(BootstrapArgs),
    /// Create the kernel's root character and bind its public key. Run once,
    /// with the service stopped, before the first start.
    Init {
        /// The character to make root. Becomes a new character, or an
        /// existing live one made root, keeping its principal id.
        #[arg(long = "as", value_name = "NAME")]
        as_name: Option<String>,
        /// The public key file to bind, in OpenSSH format.
        #[arg(long, value_name = "FILE")]
        key: Option<String>,
    },
    /// Bind a public key to an existing character. Never mints one — its
    /// principal id must already exist (`kj character create <name>`).
    AddKey {
        /// The public key file to bind, in OpenSSH format.
        pubkey_file: String,
        /// The character to bind the key to.
        #[arg(long = "as", value_name = "NAME")]
        as_name: String,
        /// Move an already-bound key to this character instead of refusing.
        #[arg(long)]
        rebind: bool,
    },
    /// List every bound fingerprint.
    ListKeys,
    /// List characters from kernel.db, read-only. Works with the service
    /// stopped — the lockout-recovery path.
    ListCharacters,
    /// One-time upgrade of a pre-melt auth.db: harvest its usernames into
    /// kernel.db as characters, then drop the columns that held them. Run
    /// once, with the service stopped. No-op if already melted.
    MigrateKeyring,
    /// Block maintenance on kernel.db. Service stopped for any change.
    Blocks {
        #[command(subcommand)]
        command: BlocksCommand,
    },
    /// rc scripts. No running kernel needed.
    Rc {
        #[command(subcommand)]
        command: RcCommand,
    },
    /// Run one kj verb against a stopped kernel: boots the same kernel the
    /// service boots, minus serving, dispatches the verb as the named root
    /// character, prints the result, settles the kernel, and exits. A verb
    /// that admits a model turn (`kj drive`) holds the command until the
    /// turn ends.
    Kj(KjCliArgs),
}

#[derive(Subcommand)]
enum RcCommand {
    /// Install the embedded rc scripts into the rc tree. Installs anything
    /// absent, and names files that differ from their default and retired
    /// defaults still installed, leaving both alone; --force overwrites the
    /// first and removes the second. A path with a <path>.keep file beside it
    /// is left as it is, present, edited, or absent, even with --force.
    Reseed {
        /// Also overwrite files that differ from their embedded default and
        /// remove retired defaults.
        #[arg(long, short = 'f')]
        force: bool,
        /// Seed this directory instead of the /config/rc tree the path
        /// flags would otherwise resolve.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum BlocksCommand {
    /// Find tool results stored before their calls (the order-key bug fixed
    /// in 855ace8a) in every conversation, archived ones included, and move
    /// each damaged turn back into tick order by giving its blocks fresh keys. Reports only, read-only,
    /// unless --apply; --apply refuses while the service holds the kernel
    /// lock. Exits nonzero while any result stays before its call.
    RepairOrder {
        /// Only contexts whose hex id starts with this prefix.
        #[arg(long, value_name = "HEX")]
        context: Option<String>,
        /// Re-key the damaged runs. Back up kernel.db first.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Args)]
struct BootstrapArgs {
    /// The root character: you. Default: $USER.
    #[arg(long = "as", value_name = "NAME")]
    as_name: Option<String>,

    /// Your public key, in OpenSSH format.
    #[arg(long, value_name = "FILE", default_value = "~/.ssh/id_ed25519.pub")]
    key: String,

    /// The provider ROOT talks to: anthropic or deepseek. Default: each
    /// provider whose key is in its environment variable
    /// (ANTHROPIC_API_KEY, DEEPSEEK_API_KEY) or its key file
    /// (~/.anthropic-key.txt, ~/.deepseek-key), the first one found as the
    /// default.
    #[arg(long, value_name = "KIND")]
    backend: Option<String>,

    /// The default model. Default: claude-sonnet-5-5 or deepseek-flash.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// The file holding the --backend provider's key, as the kernel sees it
    /// (~ expanded where the kernel runs).
    #[arg(long, value_name = "FILE", requires = "backend")]
    api_key_file: Option<String>,

    /// Set up ROOT without a model. Add one later from your root with
    /// `kj backend set`, or ask bootstrap again with a key.
    #[arg(long, conflicts_with_all = ["backend", "model"])]
    no_model: bool,

    /// The model character that plays ROOT.
    #[arg(long = "console-as", value_name = "NAME", default_value = "root")]
    console_as: String,
}

#[derive(Args)]
struct KjCliArgs {
    /// The root character to act as. Required and refused with the full
    /// list when more than one live root character exists; omit when there
    /// is exactly one.
    #[arg(long = "as", value_name = "NAME")]
    as_character: Option<String>,

    /// Run the verb from another context instead of the caller's root
    /// context — '.', a label, or a hex id prefix, the way `kj` resolves a
    /// context reference anywhere else.
    #[arg(long, value_name = "CONTEXT")]
    context: Option<String>,

    /// Print the result's structured data as JSON instead of its message.
    #[arg(long)]
    json: bool,

    /// The kj verb and its arguments, exactly as at a kj prompt — for
    /// example `character create bob --root`. Must follow `--`.
    #[arg(last = true, required = true, value_name = "ARGV")]
    argv: Vec<String>,
}

fn main() -> ExitCode {
    // The default `#[tokio::main]` runtime sizes worker threads at 2 MiB,
    // which is too small for the ROOT genesis `create` rc chain that boot
    // runs on this runtime (`run_server` → `SshServer::run` →
    // `create_shared_kernel`, `rpc.rs`) — the same class of overflow
    // `spawn_kaish_thread`'s dedicated threads exist to avoid. Size every
    // worker thread the same way. `kj` boots the same way boot does, on
    // this same runtime.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("kaijutsu-server-worker")
        .thread_stack_size(kaijutsu_kernel::KAISH_RC_THREAD_STACK)
        .enable_all()
        .build()
        .expect("build the server's tokio runtime");
    runtime.block_on(async_main())
}

async fn async_main() -> ExitCode {
    // Parsed before the logging filter: `kj` (`docs/server-cli.md`) boots the
    // same kernel the serving default boots, but its whole point is one
    // verb's answer on stdout, not a running service's narration — so it
    // defaults quieter (`kaijutsu_server::offline::default_log_directive`)
    // unless the operator set RUST_LOG explicitly.
    let cli = Cli::parse();
    let is_kj = matches!(cli.command, Some(Command::Kj(_) | Command::Bootstrap(_)));
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(kaijutsu_server::offline::default_log_directive(is_kj)));

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

    let PathArgs { config_root, mount, rw_mount, port } = cli.paths;

    match cli.command {
        None => {
            let mode = if cli.headless { KernelMode::Headless } else { KernelMode::Interactive };
            run_server(port, config_root, mount, rw_mount, mode).await
        }
        Some(Command::Bootstrap(args)) => cmd_bootstrap(args, config_root, &mount, rw_mount).await,
        Some(Command::Init { as_name, key }) => cmd_init(as_name, key),
        Some(Command::AddKey { pubkey_file, as_name, rebind }) => {
            cmd_add_key(pubkey_file, as_name, rebind)
        }
        Some(Command::ListKeys) => cmd_list_keys(),
        Some(Command::ListCharacters) => cmd_list_characters(),
        Some(Command::MigrateKeyring) => cmd_migrate_keyring(),
        Some(Command::Blocks { command: BlocksCommand::RepairOrder { context, apply } }) => {
            match kaijutsu_server::offline::repair_order(&kernel_data_dir(), context.as_deref(), apply) {
                Ok(0) => ExitCode::SUCCESS,
                Ok(_) => ExitCode::FAILURE,
                Err(e) => { eprintln!("blocks repair-order: {e}"); ExitCode::FAILURE }
            }
        }
        Some(Command::Rc { command: RcCommand::Reseed { force, dir } }) => {
            cmd_rc_reseed(force, dir, config_root, &mount)
        }
        Some(Command::Kj(args)) => cmd_kj(args, config_root, &mount, rw_mount).await,
    }
}

/// The default `kernel.db` path — same default the running server's own
/// bootstrap uses (`kaijutsu_server::rpc::kernel_data_dir`), so `add-key
/// --as` and `list-characters` resolve a name against the exact file a live
/// server would.
fn default_kernel_db_path() -> PathBuf {
    kernel_data_dir().join("kernel.db")
}

/// Resolve the mount registry the path flags describe: `--config-root`
/// (falling back to the XDG default), then `<config-root>/mounts.toml`, then
/// `--mount` flags, each beating the last.
fn into_mounts(config_root: Option<PathBuf>, mount: &[String]) -> Result<ConfigMounts, String> {
    let mut mounts = ConfigMounts::new(config_root.unwrap_or_else(ConfigMounts::default_root));
    mounts.load_declarations()?;
    for arg in mount {
        mounts.set_from_arg(arg)?;
    }
    Ok(mounts)
}

async fn run_server(
    port: u16,
    config_root: Option<PathBuf>,
    mount: Vec<String>,
    rw_mount: Vec<PathBuf>,
    mode: KernelMode,
) -> ExitCode {
    // Resolve the config mounts BEFORE announcing a start. A bad declaration
    // is a refusal to boot, and saying "Starting..." first would report a
    // server that came up and died rather than one that never began.
    let mounts = match into_mounts(config_root, &mount) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("config mounts: {e}");
            return ExitCode::FAILURE;
        }
    };

    tracing::info!("Starting kaijutsu server on SSH port {}...", port);
    let mut config = SshServerConfig::production(port);
    config.rw_mounts = match mounts.rw_mounts_with(&rw_mount) {
        Ok(rw) => rw,
        Err(e) => {
            eprintln!("config mounts: {e}");
            return ExitCode::FAILURE;
        }
    };
    config.config_mounts = mounts;
    config.mode = mode;
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
fn cmd_rc_reseed(
    force: bool,
    dir: Option<PathBuf>,
    config_root: Option<PathBuf>,
    mount: &[String],
) -> ExitCode {
    // Without `--dir`, reseed the rc tree the server itself would mount from
    // `--config-root`/`--mount` (`docs/config-namespace.md`).
    let root = match dir {
        Some(dir) => dir,
        None => match into_mounts(config_root, mount) {
            Ok(mounts) => mounts.host_dir(kaijutsu_types::paths::RC_ROOT),
            Err(e) => {
                eprintln!("config mounts: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
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
                println!(
                    "\nPass --force to overwrite them with the embedded defaults. To keep \
                     one through --force, create <path>.keep beside it."
                );
            }
            if !r.kept.is_empty() {
                println!(
                    "\n{} file(s) have a .keep file and were left as they are:",
                    r.kept.len()
                );
                for path in &r.kept {
                    println!("  {path}");
                }
            }
            if !r.retired.is_empty() {
                println!(
                    "\n{} retired default(s) are still installed and were left alone:",
                    r.retired.len()
                );
                for path in &r.retired {
                    println!("  {path}");
                }
                println!("\nPass --force to remove them, or delete them by hand.");
            }
            if !r.removed.is_empty() {
                println!("\n{} retired default(s) removed:", r.removed.len());
                for path in &r.removed {
                    println!("  {path}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rc reseed into {} failed: {e}", root.display());
            ExitCode::FAILURE
        }
    }
}

const INIT_USAGE: &str = "Usage: kaijutsu-server init --as <name> --key <pubkey-file>";

/// Create the kernel's root character and bind its key
/// (`kaijutsu_server::init`). Run with the service stopped.
fn cmd_init(as_name: Option<String>, key: Option<String>) -> ExitCode {
    let (name, key_file) = match (as_name, key) {
        (Some(name), Some(key_file)) => (name, key_file),
        _ => {
            eprintln!("{INIT_USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let key_path: PathBuf = shellexpand::tilde(&key_file).as_ref().into();
    let key_data = match std::fs::read_to_string(&key_path) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Failed to read {}: {e}", key_path.display());
            return ExitCode::FAILURE;
        }
    };
    let key = match ssh_key::PublicKey::from_openssh(key_data.trim()) {
        Ok(key) => key,
        Err(e) => {
            eprintln!("Failed to parse public key {}: {e}", key_path.display());
            return ExitCode::FAILURE;
        }
    };
    let comment = extract_comment(key_data.trim());

    let kernel_db_path = default_kernel_db_path();
    let kernel_db = match KernelDb::open(&kernel_db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open {}: {e}", kernel_db_path.display());
            return ExitCode::FAILURE;
        }
    };
    let auth_db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {e}");
            return ExitCode::FAILURE;
        }
    };

    match kaijutsu_server::init::init_root(&kernel_db, &auth_db, &name, &key, comment.as_deref()) {
        Ok(report) => {
            let character = if report.changed_character { "is now" } else { "was already" };
            let key = if report.bound_key { "bound" } else { "was already bound" };
            println!(
                "{name} ({}) {character} the root character; key {} {key} to it.\n\
                 Start the server; it creates the root context '{name}'.",
                report.character.principal_id.short(),
                report.fingerprint,
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("init: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Bind a public key to an existing character. Never mints: the character's
/// principal id must already exist in `kernel.db` (`kj character create
/// <name>`), and this only writes `auth.db`
/// (`docs/character.md`, "Adding a key binds; it never mints").
fn cmd_add_key(key_file: String, character: String, rebind: bool) -> ExitCode {
    // Expand path (handle ~)
    let key_path: PathBuf = shellexpand::tilde(&key_file).as_ref().into();

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
                 (run `kaijutsu-server init --as <name> --key <pubkey-file>` first)",
                kernel_db_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let character = match kdb.get_character_by_name(&character) {
        Ok(Some(row)) => row,
        Ok(None) => {
            eprintln!(
                "No character named '{}' — `kaijutsu-server list-characters` to see who exists",
                character
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
        if !rebind {
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
                 (run `kaijutsu-server init --as <name> --key <pubkey-file>` first)",
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
///
/// Unlike `init`, `add-key`, `list-keys`, and `list-characters` — the direct-
/// database lockout tools, which stay lock-free so they still work when the
/// kernel refuses to boot — this writes `kernel.db` read-write, so it takes
/// the same `KernelLock` `kj` and the service take, refusing at once if
/// either already holds it.
fn cmd_migrate_keyring() -> ExitCode {
    let data_dir = kernel_data_dir();
    let _lock = match kaijutsu_server::offline::KernelLock::acquire(&data_dir) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("migrate-keyring: {e}");
            return ExitCode::FAILURE;
        }
    };
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

/// Set up the kernel so `ROOT` can answer (`kaijutsu_server::bootstrap`).
async fn cmd_bootstrap(
    args: BootstrapArgs,
    config_root: Option<PathBuf>,
    mount: &[String],
    rw_mount: Vec<PathBuf>,
) -> ExitCode {
    match bootstrap_options(args) {
        Ok(opts) => {
            let mut config = SshServerConfig::production(DEFAULT_SSH_PORT);
            config.config_mounts = match into_mounts(config_root, mount) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("config mounts: {e}");
                    return ExitCode::FAILURE;
                }
            };
            config.rw_mounts = match config.config_mounts.rw_mounts_with(&rw_mount) {
                Ok(rw) => rw,
                Err(e) => {
                    eprintln!("config mounts: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match kaijutsu_server::bootstrap::bootstrap(&config, &opts).await {
                Ok(report) => {
                    for line in &report.ran {
                        println!("ran: {line}");
                    }
                    for note in &report.notes {
                        println!("{note}");
                    }
                    println!("Start the server and connect as {}; ROOT is where to begin.", opts.root);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("bootstrap: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            eprintln!("bootstrap: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the flags and the environment into what to set up.
fn bootstrap_options(args: BootstrapArgs) -> Result<kaijutsu_server::bootstrap::BootstrapOptions, String> {
    use kaijutsu_server::bootstrap::{BackendChoice, BootstrapOptions, detect_backends};

    let root = match args.as_name.or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty())) {
        Some(name) => name,
        None => return Err("$USER is unset; name the root character with --as <name>".to_string()),
    };
    let key_path: PathBuf = shellexpand::tilde(&args.key).as_ref().into();
    let key_data = std::fs::read_to_string(&key_path)
        .map_err(|e| format!("read {}: {e}; name your public key with --key <file>", key_path.display()))?;
    let root_key = ssh_key::PublicKey::from_openssh(key_data.trim())
        .map_err(|e| format!("parse the public key {}: {e}", key_path.display()))?;

    let backends = match args.backend {
        Some(kind) => {
            let mut backend = BackendChoice::provider(&kind)
                .ok_or_else(|| format!("--backend takes anthropic or deepseek, not '{kind}'"))?;
            if let Some(model) = args.model {
                backend.model = model;
            }
            if let Some(file) = args.api_key_file {
                backend.api_key_file = Some(file);
            }
            vec![backend]
        }
        None if args.no_model => Vec::new(),
        None => {
            let mut found = detect_backends(
                |var| std::env::var(var).is_ok_and(|v| !v.trim().is_empty()),
                |file| PathBuf::from(shellexpand::tilde(file).as_ref()).is_file(),
            );
            match (args.model, found.first_mut()) {
                (Some(model), Some(first)) => first.model = model,
                (Some(model), None) => {
                    return Err(format!(
                        "--model {model} needs a provider, and no key was found; name one with --backend"
                    ));
                }
                (None, _) => {}
            }
            found
        }
    };

    Ok(BootstrapOptions {
        root,
        root_key,
        key_comment: extract_comment(key_data.trim()),
        backends,
        console_performer: args.console_as,
    })
}

/// Run one `kj` verb against a stopped kernel (`docs/server-cli.md`).
async fn cmd_kj(
    args: KjCliArgs,
    config_root: Option<PathBuf>,
    mount: &[String],
    rw_mount: Vec<PathBuf>,
) -> ExitCode {
    let config_mounts = match into_mounts(config_root, mount) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("config mounts: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rw_mounts = match config_mounts.rw_mounts_with(&rw_mount) {
        Ok(rw) => rw,
        Err(e) => {
            eprintln!("config mounts: {e}");
            return ExitCode::FAILURE;
        }
    };
    run_kj(KjRunArgs {
        config_dir: None,
        config_mounts,
        data_dir: None,
        rw_mounts,
        as_character: args.as_character,
        context: args.context,
        json: args.json,
        argv: args.argv,
    })
    .await
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
