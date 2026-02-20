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
//! # Key management
//! kaijutsu-server add-key <pubkey-file> [--nick NAME] [--admin]
//! kaijutsu-server list-users
//! kaijutsu-server list-keys [nick]
//! kaijutsu-server import <authorized_keys_file>
//! kaijutsu-server set-nick <old> <new>
//! ```

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use kaijutsu_server::constants::DEFAULT_SSH_PORT;
use kaijutsu_server::{AuthDb, SshServer, SshServerConfig};
use russh::keys::ssh_key::{self, HashAlg};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn print_usage() {
    eprintln!(
        r#"kaijutsu-server - SSH + Cap'n Proto server for kaijutsu

USAGE:
    kaijutsu-server [OPTIONS] [COMMAND]

COMMANDS:
    (default)                     Run the SSH server
    add-key <file> [OPTIONS]      Add an SSH public key
    remove-user <nick>            Remove a user and all their keys
    list-users                    List all users
    list-keys [nick]              List keys (all or for a specific user)
    import <file>                 Import keys from authorized_keys file
    set-nick <old> <new>          Rename a user

OPTIONS:
    --port <PORT>                 SSH port (default: {port})
    --nick <NAME>                 Nickname for the key (default: derived from fingerprint)
    --admin                       Grant admin privileges
    --help, -h                    Show this help

EXAMPLES:
    kaijutsu-server                           # Run server on port {port}
    kaijutsu-server --port 2222               # Run server on port 2222
    kaijutsu-server add-key ~/.ssh/id_ed25519.pub --nick amy --admin
    kaijutsu-server import ~/.ssh/authorized_keys
    kaijutsu-server list-users
    kaijutsu-server list-keys amy
    kaijutsu-server set-nick xyz789ab amy
    kaijutsu-server remove-user olduser

DATABASE:
    Keys are stored in: {db_path}
"#,
        port = DEFAULT_SSH_PORT,
        db_path = AuthDb::default_path().display()
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

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
        return run_server(DEFAULT_SSH_PORT).await;
    }

    match args[1].as_str() {
        "--help" | "-h" => {
            print_usage();
            ExitCode::SUCCESS
        }
        "--port" => {
            let port = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_SSH_PORT);
            run_server(port).await
        }
        "add-key" => cmd_add_key(&args[2..]),
        "remove-user" => cmd_remove_user(&args[2..]),
        "list-users" => cmd_list_users(),
        "list-keys" => cmd_list_keys(&args[2..]),
        "import" => cmd_import(&args[2..]),
        "set-nick" => cmd_set_nick(&args[2..]),
        arg => {
            // Try parsing as port number for backwards compatibility
            if let Ok(port) = arg.parse::<u16>() {
                return run_server(port).await;
            }
            eprintln!("Unknown command: {}", arg);
            print_usage();
            ExitCode::FAILURE
        }
    }
}

async fn run_server(port: u16) -> ExitCode {
    tracing::info!("Starting kaijutsu server on SSH port {}...", port);

    let config = SshServerConfig::production(port);
    let server = SshServer::new(config);

    if let Err(e) = server.run().await {
        tracing::error!("Server error: {}", e);
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Add a public key to the database
fn cmd_add_key(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Usage: kaijutsu-server add-key <pubkey-file> [--nick NAME] [--admin]");
        return ExitCode::FAILURE;
    }

    let key_file = &args[0];
    let mut nick: Option<&str> = None;
    let mut is_admin = false;

    // Parse options
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--nick" => {
                if i + 1 < args.len() {
                    nick = Some(&args[i + 1]);
                    i += 2;
                } else {
                    eprintln!("--nick requires a value");
                    return ExitCode::FAILURE;
                }
            }
            "--admin" => {
                is_admin = true;
                i += 1;
            }
            other => {
                eprintln!("Unknown option: {}", other);
                return ExitCode::FAILURE;
            }
        }
    }

    // Expand path (handle ~)
    let key_path: PathBuf = shellexpand::tilde(key_file).as_ref().into();

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

    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

    // Extract comment from key data
    let comment = extract_comment(key_data.trim());

    // Open database
    let mut db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    // Check if key already exists
    match db.get_key(&fingerprint) {
        Ok(Some(existing)) => {
            eprintln!("Key already exists: {}", fingerprint);
            if let Ok(Some(user)) = db.get_user(existing.user_id) {
                eprintln!("  User: {} ({})", user.nick, user.display_name);
            }
            return ExitCode::FAILURE;
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("Database error: {}", e);
            return ExitCode::FAILURE;
        }
    }

    // Add the key
    match db.add_key_auto_user(&key, comment.as_deref(), nick, is_admin) {
        Ok((user_id, _key_id)) => {
            if let Ok(Some(user)) = db.get_user(user_id) {
                println!("Added key for user '{}':", user.nick);
                println!("  Fingerprint: {}", fingerprint);
                println!("  Display name: {}", user.display_name);
                println!("  Admin: {}", user.is_admin);
            } else {
                println!("Added key: {}", fingerprint);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Failed to add key: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// Remove a user and all their keys
fn cmd_remove_user(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Usage: kaijutsu-server remove-user <nick>");
        return ExitCode::FAILURE;
    }

    let nick = &args[0];

    let db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    // Check if user exists first
    match db.get_user_by_nick(nick) {
        Ok(Some(user)) => {
            // Get key count for confirmation message
            let key_count = db.list_keys(user.id).map(|k| k.len()).unwrap_or(0);

            match db.remove_user(nick) {
                Ok(true) => {
                    println!(
                        "Removed user '{}' ({}) and {} key(s)",
                        nick, user.display_name, key_count
                    );
                    ExitCode::SUCCESS
                }
                Ok(false) => {
                    eprintln!("User not found: {}", nick);
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("Failed to remove user: {}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Ok(None) => {
            eprintln!("User not found: {}", nick);
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Database error: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// List all users
fn cmd_list_users() -> ExitCode {
    let db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let users = match db.list_users() {
        Ok(users) => users,
        Err(e) => {
            eprintln!("Failed to list users: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if users.is_empty() {
        println!("No users found. Add keys with: kaijutsu-server add-key <pubkey>");
        return ExitCode::SUCCESS;
    }

    println!("{:<16} {:<24} {:>5}", "NICK", "DISPLAY NAME", "ADMIN");
    println!("{}", "-".repeat(48));

    for user in users {
        let admin_str = if user.is_admin { "yes" } else { "" };
        println!(
            "{:<16} {:<24} {:>5}",
            user.nick, user.display_name, admin_str
        );
    }

    ExitCode::SUCCESS
}

/// List keys (all or for a specific user)
fn cmd_list_keys(args: &[String]) -> ExitCode {
    let db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if let Some(nick) = args.first() {
        // List keys for specific user
        let user = match db.get_user_by_nick(nick) {
            Ok(Some(user)) => user,
            Ok(None) => {
                eprintln!("User not found: {}", nick);
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("Database error: {}", e);
                return ExitCode::FAILURE;
            }
        };

        let keys = match db.list_keys(user.id) {
            Ok(keys) => keys,
            Err(e) => {
                eprintln!("Failed to list keys: {}", e);
                return ExitCode::FAILURE;
            }
        };

        println!("Keys for {} ({}):", user.nick, user.display_name);
        println!();

        for key in keys {
            println!("  {} {}", key.key_type, key.fingerprint);
            if let Some(comment) = &key.comment {
                println!("    Comment: {}", comment);
            }
            if let Some(last_used) = key.last_used_at {
                println!("    Last used: {}", format_timestamp(last_used));
            }
        }
    } else {
        // List all keys
        let all_keys = match db.list_all_keys() {
            Ok(keys) => keys,
            Err(e) => {
                eprintln!("Failed to list keys: {}", e);
                return ExitCode::FAILURE;
            }
        };

        if all_keys.is_empty() {
            println!("No keys found. Add keys with: kaijutsu-server add-key <pubkey>");
            return ExitCode::SUCCESS;
        }

        println!(
            "{:<16} {:<12} {:<48} {}",
            "USER", "TYPE", "FINGERPRINT", "COMMENT"
        );
        println!("{}", "-".repeat(90));

        for (user, key) in all_keys {
            let comment = key.comment.as_deref().unwrap_or("");
            println!(
                "{:<16} {:<12} {:<48} {}",
                user.nick, key.key_type, key.fingerprint, comment
            );
        }
    }

    ExitCode::SUCCESS
}

/// Import keys from authorized_keys file
fn cmd_import(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("Usage: kaijutsu-server import <authorized_keys_file>");
        return ExitCode::FAILURE;
    }

    let file = &args[0];
    let path: PathBuf = shellexpand::tilde(file).as_ref().into();

    let mut db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    // First key becomes admin if database is empty
    let first_is_admin = db.is_empty().unwrap_or(false);

    match db.import_authorized_keys(&path, first_is_admin) {
        Ok(count) => {
            println!("Imported {} key(s) from {}", count, path.display());
            if first_is_admin && count > 0 {
                println!("First key was granted admin privileges.");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Failed to import keys: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// Rename a user
fn cmd_set_nick(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("Usage: kaijutsu-server set-nick <old-nick> <new-nick>");
        return ExitCode::FAILURE;
    }

    let old_nick = &args[0];
    let new_nick = &args[1];

    let db = match AuthDb::open(AuthDb::default_path()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open auth database: {}", e);
            return ExitCode::FAILURE;
        }
    };

    match db.set_nick(old_nick, new_nick) {
        Ok(true) => {
            println!("Renamed user '{}' to '{}'", old_nick, new_nick);
            ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("User not found: {}", old_nick);
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Failed to rename user: {}", e);
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

/// Format a Unix timestamp for display
fn format_timestamp(ts: i64) -> String {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let time = UNIX_EPOCH + Duration::from_secs(ts as u64);
    let now = SystemTime::now();

    match now.duration_since(time) {
        Ok(elapsed) => {
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("{}s ago", secs)
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        }
        Err(_) => "in the future".to_string(),
    }
}
