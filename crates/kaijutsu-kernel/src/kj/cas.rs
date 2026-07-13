use clap::{Parser, Subcommand};
use kaijutsu_cas::ContentStore;
use kaijutsu_types::ContentType;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "cas",
    about = "Content-addressed storage for binary blobs (images, etc.)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CasArgs {
    #[command(subcommand)]
    command: CasCommand,
}

#[derive(Subcommand, Debug)]
enum CasCommand {
    /// Ingest a file, print its hash.
    Put {
        /// Path to the file to ingest
        path: String,
    },
    /// Retrieve by hash. With `--out`, write the bytes to a file; otherwise
    /// report the size (binary data can't go to stdout as text).
    Get {
        /// Content hash to retrieve
        hash: String,
        /// Write the retrieved bytes to this path instead of reporting size
        #[arg(long)]
        out: Option<String>,
    },
    /// List all stored objects.
    #[command(alias = "list")]
    Ls,
    /// Show metadata (mime, size, path) for a hash.
    Info {
        /// Content hash to inspect
        hash: String,
    },
    /// Remove an object (unconditional, no ref-checking).
    #[command(alias = "remove")]
    Rm {
        /// Content hash to remove
        hash: String,
    },
}

impl KjDispatcher {
    pub(crate) fn dispatch_cas(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CasArgs>();
        }
        let parsed = match CasArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj cas: {e}"));
            }
        };

        // Writing/removing blobs is operator authority; get/ls/info stay ungated.
        if matches!(parsed.command, CasCommand::Put { .. } | CasCommand::Rm { .. })
            && let Err(denied) =
                self.require_cap(caller, crate::mcp::Capability::Operator, "cas")
        {
            return denied;
        }

        match parsed.command {
            CasCommand::Put { path } => self.cas_put(&path),
            CasCommand::Get { hash, out } => self.cas_get(&hash, out.as_deref()),
            CasCommand::Ls => self.cas_ls(),
            CasCommand::Info { hash } => self.cas_info(&hash),
            CasCommand::Rm { hash } => self.cas_rm(&hash),
        }
    }

    /// Ingest a HOST filesystem path (`kj cas put` has always taken one —
    /// not a VFS path) with bounded memory: read in
    /// `vfs::STREAM_CHUNK_SIZE` pieces straight into a
    /// [`kaijutsu_cas::StreamingWriter`], hashing incrementally, rather than
    /// buffering the whole file before `store()`. This is a self-contained
    /// swap of `cas_put`'s internals — NOT routed through `VfsOps`/`vfs::pump`,
    /// since making `kj cas put` accept a VFS path (so it could one day reach
    /// a share under `/r/<id>/...`) is a slice-1 concern once `ShareFs`
    /// exists (`docs/slash-r.md`), not a slice-0 restructuring.
    fn cas_put(&self, path_str: &str) -> KjResult {
        use std::io::Read;

        let path = std::path::Path::new(path_str);
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => return KjResult::Err(format!("kj cas put: {}: {}", path_str, e)),
        };

        let mime = mime_from_extension(path_str);
        let cas = self.kernel().cas();
        let mut writer = match cas.create_streaming_writer(mime) {
            Ok(w) => w,
            Err(e) => return KjResult::Err(format!("kj cas put: {}", e)),
        };

        let mut buf = vec![0u8; crate::vfs::STREAM_CHUNK_SIZE as usize];
        loop {
            let n = match file.read(&mut buf) {
                Ok(n) => n,
                Err(e) => return KjResult::Err(format!("kj cas put: {}: {}", path_str, e)),
            };
            if n == 0 {
                break;
            }
            if let Err(e) = writer.write(&buf[..n]) {
                return KjResult::Err(format!("kj cas put: {}", e));
            }
        }

        match writer.finalize() {
            Ok(result) => KjResult::ok(result.content_hash.to_string()),
            Err(e) => KjResult::Err(format!("kj cas put: {}", e)),
        }
    }

    fn cas_get(&self, hash_str: &str, out: Option<&str>) -> KjResult {
        let hash = match hash_str.parse::<kaijutsu_cas::ContentHash>() {
            Ok(h) => h,
            Err(e) => return KjResult::Err(format!("kj cas get: invalid hash: {}", e)),
        };

        let cas = self.kernel().cas();
        let data = match cas.retrieve(&hash) {
            Ok(Some(d)) => d,
            Ok(None) => return KjResult::Err(format!("kj cas get: not found: {}", hash)),
            Err(e) => return KjResult::Err(format!("kj cas get: {}", e)),
        };

        // --out <path>: write to file. Clap binds it regardless of position,
        // so the old `argv[1] == "--out"` positional fragility is gone.
        if let Some(out_path) = out {
            return match std::fs::write(out_path, &data) {
                Ok(()) => KjResult::ok_ephemeral(
                    format!("wrote {} bytes to {}", data.len(), out_path),
                    ContentType::Plain,
                ),
                Err(e) => KjResult::Err(format!("kj cas get --out: {}", e)),
            };
        }

        // Default: report size (binary data can't meaningfully go to stdout as text)
        KjResult::ok_ephemeral(format!("{} bytes", data.len()), ContentType::Plain)
    }

    fn cas_ls(&self) -> KjResult {
        let cas = self.kernel().cas();
        let objects_dir = cas.config().objects_dir();

        let empty_data = serde_json::Value::Array(Vec::new());
        let prefix_dirs = match std::fs::read_dir(&objects_dir) {
            Ok(d) => d,
            Err(_) => {
                return KjResult::ok_ephemeral_with_data(
                    "(empty)",
                    ContentType::Plain,
                    empty_data,
                );
            }
        };

        // Collect (hash, formatted line) pairs so `.data` can carry full
        // hashes while the text view keeps size/mime columns.
        let mut rows: Vec<(String, String)> = Vec::new();
        for prefix_entry in prefix_dirs.flatten() {
            if !prefix_entry.path().is_dir() {
                continue;
            }
            let prefix = prefix_entry.file_name().to_string_lossy().to_string();
            if let Ok(files) = std::fs::read_dir(prefix_entry.path()) {
                for file_entry in files.flatten() {
                    let remainder = file_entry.file_name().to_string_lossy().to_string();
                    let hash_str = format!("{}{}", prefix, remainder);
                    if let Ok(hash) = hash_str.parse::<kaijutsu_cas::ContentHash>() {
                        let (size, mime) = match cas.inspect(&hash) {
                            Ok(Some(r)) => (r.size_bytes, r.mime_type),
                            _ => {
                                let size = file_entry.metadata().map(|m| m.len()).unwrap_or(0);
                                (size, "?".into())
                            }
                        };
                        let hash_full = hash.to_string();
                        let line = format!("{}  {:>8}  {}", hash_full, size, mime);
                        rows.push((hash_full, line));
                    }
                }
            }
        }

        rows.sort_by(|a, b| a.0.cmp(&b.0));
        // Iteration handles: full content hashes. `cas info <hash>` and
        // `cas get <hash>` both accept the full form.
        let hashes = serde_json::Value::Array(
            rows.iter()
                .map(|(h, _)| serde_json::Value::String(h.clone()))
                .collect(),
        );
        let text = if rows.is_empty() {
            "(empty)".to_string()
        } else {
            rows.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n")
        };
        KjResult::ok_ephemeral_with_data(text, ContentType::Plain, hashes)
    }

    fn cas_info(&self, hash_str: &str) -> KjResult {
        let hash = match hash_str.parse::<kaijutsu_cas::ContentHash>() {
            Ok(h) => h,
            Err(e) => return KjResult::Err(format!("kj cas info: invalid hash: {}", e)),
        };

        let cas = self.kernel().cas();
        match cas.inspect(&hash) {
            Ok(Some(r)) => {
                let mut lines = vec![
                    format!("hash:  {}", r.hash),
                    format!("mime:  {}", r.mime_type),
                    format!("size:  {} bytes", r.size_bytes),
                ];
                if let Some(path) = r.local_path {
                    lines.push(format!("path:  {}", path));
                }
                KjResult::ok_ephemeral(lines.join("\n"), ContentType::Plain)
            }
            Ok(None) => KjResult::Err(format!("kj cas info: not found: {}", hash)),
            Err(e) => KjResult::Err(format!("kj cas info: {}", e)),
        }
    }

    fn cas_rm(&self, hash_str: &str) -> KjResult {
        let hash = match hash_str.parse::<kaijutsu_cas::ContentHash>() {
            Ok(h) => h,
            Err(e) => return KjResult::Err(format!("kj cas rm: invalid hash: {}", e)),
        };

        let cas = self.kernel().cas();
        match cas.remove(&hash) {
            Ok(true) => KjResult::ok_ephemeral(format!("removed {}", hash), ContentType::Plain),
            Ok(false) => KjResult::Err(format!("kj cas rm: not found: {}", hash)),
            Err(e) => KjResult::Err(format!("kj cas rm: {}", e)),
        }
    }

}

pub fn mime_from_extension(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".avif") {
        "image/avif"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".wav") {
        "audio/wav"
    } else if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use std::sync::Arc;

    /// `cas get --out <path> <hash>` must bind regardless of flag/positional
    /// order. The old hand-parser read `argv[1] == "--out"`, so the flag-first
    /// form fed "--out" to the hash parser and failed. Clap binds either order —
    /// this is the order-independence the migration buys. Fails red if anyone
    /// reverts `cas_get` to positional-index `--out` handling.
    #[tokio::test]
    async fn cas_get_out_flag_before_positional() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        // Seed a blob via `cas put` of a temp file; capture its hash.
        let dir = tempfile::tempdir().expect("tmpdir");
        let src = dir.path().join("blob.bin");
        std::fs::write(&src, b"hello cas").expect("write src");
        let put = dispatcher.dispatch_cas(
            &["put".to_string(), src.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(put.is_ok(), "cas put failed: {put:?}");
        let hash = put.message().to_string();

        // Flag-first: `get --out <path> <hash>` — the form the old parser broke.
        let out = dir.path().join("out.bin");
        let res = dispatcher.dispatch_cas(
            &[
                "get".to_string(),
                "--out".to_string(),
                out.to_string_lossy().into_owned(),
                hash,
            ],
            &caller,
        );
        assert!(res.is_ok(), "cas get --out (flag first) failed: {res:?}");
        let got = std::fs::read(&out).expect("out file written");
        assert_eq!(got, b"hello cas", "round-tripped bytes must match");
    }

    /// Command aliases route to the same leaf: `list`→`ls`, `remove`→`rm`.
    /// Fails red if the aliases drop off the clap subcommands.
    #[tokio::test]
    async fn cas_aliases_route() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        // `list` is an alias of `ls` — empty store lists cleanly (is_ok).
        let res = dispatcher.dispatch_cas(&["list".to_string()], &caller);
        assert!(res.is_ok(), "cas list (alias of ls) failed: {res:?}");

        // `remove <hash>` is an alias of `rm`; removing an absent hash is a
        // clean error from cas_rm (not an unknown-subcommand error), proving
        // the alias routed to the rm leaf.
        let res = dispatcher.dispatch_cas(
            &["remove".to_string(), "0".repeat(64)],
            &caller,
        );
        assert!(!res.is_ok(), "remove of absent hash should error: {res:?}");
        assert!(
            res.message().contains("cas rm"),
            "alias `remove` must route to cas_rm, got: {res:?}"
        );
    }
}
