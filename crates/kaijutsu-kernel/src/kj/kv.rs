//! `kj kv` — the human/agent CLI over the kernel key–value store.
//!
//! The KV is the kernel's persistent, synced `env` (see `docs/kernel-kv.md`):
//! flat UTF-8 keys (dotted namespaces by convention), string values, no per-key
//! ACLs. This namespace is the shell surface over it.
//!
//! ```text
//! kj kv get <key>
//! kj kv set <key> <value> [--expires-at <ms>]
//! kj kv delete <key>
//! kj kv keys [<prefix>] [--json]
//! ```

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use serde::Serialize;

use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "kv",
    about = "Kernel key–value store (persistent, synced env)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct KvArgs {
    #[command(subcommand)]
    command: KvCommand,
}

#[derive(Subcommand, Debug)]
enum KvCommand {
    /// Read a key. Errors if the key is absent, deleted, or advisory-expired.
    Get {
        /// The key to read.
        key: String,
    },
    /// Set a key to a value (last-write-wins).
    Set {
        /// The key to write.
        key: String,
        /// The value (structured data is the caller's JSON).
        value: String,
        /// Advisory absolute expiry, ms since the Unix epoch on the writer's
        /// clock. Best-effort: readers MAY treat the key as gone after it; there
        /// is no sweeper.
        #[arg(long = "expires-at")]
        expires_at: Option<i64>,
    },
    /// Delete a key. Reports whether a live value existed.
    #[command(alias = "rm")]
    Delete {
        /// The key to delete.
        key: String,
    },
    /// List keys, optionally filtered by a prefix.
    #[command(alias = "ls")]
    Keys {
        /// Only keys starting with this prefix (omit for all).
        prefix: Option<String>,
        /// Emit a JSON array instead of a newline list.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Serialize)]
struct KvGetRecord {
    key: String,
    value: Option<String>,
    found: bool,
}

#[derive(Serialize)]
struct KvSetRecord {
    key: String,
    value: String,
}

#[derive(Serialize)]
struct KvDeleteRecord {
    key: String,
    existed: bool,
}

impl KjDispatcher {
    pub(crate) fn dispatch_kv(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            let mut cmd = <KvArgs as clap::CommandFactory>::command();
            return KjResult::ok_ephemeral(cmd.render_help().to_string(), ContentType::Plain);
        }
        let parsed = match KvArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj kv: {e}"));
            }
        };

        // No capability gate: the KV is a shared-trust env, deliberately
        // ACL-free (docs/kernel-kv.md). Any caller that can run `kj` can read
        // and write it, the same as shell vars.
        let Some(kv) = self.kernel().kv() else {
            return KjResult::Err(
                "kj kv: kernel KV store not initialized (embedded/test kernel?)".to_string(),
            );
        };

        match parsed.command {
            KvCommand::Get { key } => match kv.get(&key) {
                Ok(Some(value)) => {
                    let record = KvGetRecord {
                        key,
                        value: Some(value.clone()),
                        found: true,
                    };
                    // Value verbatim + trailing newline, like a shell `echo`.
                    KjResult::ok_with_data(
                        format!("{value}\n"),
                        serde_json::to_value(&record).unwrap_or_default(),
                    )
                }
                Ok(None) => KjResult::Err(format!("kj kv get: no such key '{key}'")),
                Err(e) => KjResult::Err(format!("kj kv get: {e}")),
            },
            KvCommand::Set {
                key,
                value,
                expires_at,
            } => match kv.set(&key, &value, expires_at) {
                Ok(()) => {
                    let record = KvSetRecord {
                        key: key.clone(),
                        value: value.clone(),
                    };
                    KjResult::ok_with_data(
                        format!("set {key}\n"),
                        serde_json::to_value(&record).unwrap_or_default(),
                    )
                }
                Err(e) => KjResult::Err(format!("kj kv set: {e}")),
            },
            KvCommand::Delete { key } => match kv.delete(&key) {
                Ok(existed) => {
                    let record = KvDeleteRecord {
                        key: key.clone(),
                        existed,
                    };
                    let msg = if existed {
                        format!("deleted {key}\n")
                    } else {
                        format!("no such key '{key}' (nothing deleted)\n")
                    };
                    KjResult::ok_with_data(msg, serde_json::to_value(&record).unwrap_or_default())
                }
                Err(e) => KjResult::Err(format!("kj kv delete: {e}")),
            },
            KvCommand::Keys { prefix, json } => {
                let page = kv.keys(prefix.as_deref(), None, None);
                let id_array = serde_json::to_value(&page.keys).unwrap_or_default();
                if json {
                    return KjResult::ok_with_data(format!("{id_array}\n"), id_array);
                }
                let out = if page.keys.is_empty() {
                    "(no keys)\n".to_string()
                } else {
                    format!("{}\n", page.keys.join("\n"))
                };
                KjResult::ok_with_data(out, id_array)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{test_caller, test_dispatcher};
    use super::super::KjResult;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn run(d: &super::KjDispatcher, parts: &[&str]) -> KjResult {
        d.dispatch_kv(&argv(parts), &test_caller())
    }

    #[tokio::test]
    async fn set_get_delete_keys_roundtrip() {
        let d = test_dispatcher().await;

        // set
        let r = run(&d, &["set", "app.ctx", "abc123"]);
        assert!(matches!(r, KjResult::Ok { .. }), "set should succeed: {r:?}");

        // get returns the value verbatim
        match run(&d, &["get", "app.ctx"]) {
            KjResult::Ok { message, .. } => assert_eq!(message, "abc123\n"),
            other => panic!("expected value, got {other:?}"),
        }

        // keys lists it (prefix-filtered)
        match run(&d, &["keys", "app."]) {
            KjResult::Ok { message, .. } => assert!(message.contains("app.ctx")),
            other => panic!("expected keys, got {other:?}"),
        }

        // delete reports existence
        match run(&d, &["delete", "app.ctx"]) {
            KjResult::Ok { message, .. } => assert!(message.contains("deleted")),
            other => panic!("expected delete ok, got {other:?}"),
        }

        // get on a missing key is an error
        assert!(matches!(run(&d, &["get", "app.ctx"]), KjResult::Err(_)));
    }

    #[tokio::test]
    async fn keys_json_is_an_array() {
        let d = test_dispatcher().await;
        run(&d, &["set", "a", "1"]);
        run(&d, &["set", "b", "2"]);
        match run(&d, &["keys", "--json"]) {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("array");
                assert_eq!(arr.len(), 2);
            }
            other => panic!("expected json data, got {other:?}"),
        }
    }
}
