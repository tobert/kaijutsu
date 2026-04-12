use kaijutsu_cas::ContentStore;
use kaijutsu_types::ContentType;

use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_cas(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::ok_ephemeral(self.cas_help(), ContentType::Markdown);
        }

        match argv[0].as_str() {
            "put" => self.cas_put(&argv[1..]),
            "get" => self.cas_get(&argv[1..]),
            "ls" | "list" => self.cas_ls(),
            "info" => self.cas_info(&argv[1..]),
            "rm" | "remove" => self.cas_rm(&argv[1..]),
            "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(self.cas_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj cas: unknown subcommand '{}'\n\n{}",
                other,
                self.cas_help()
            )),
        }
    }

    fn cas_put(&self, argv: &[String]) -> KjResult {
        let path_str = match argv.first() {
            Some(p) => p,
            None => return KjResult::Err("usage: kj cas put <path>".into()),
        };

        let path = std::path::Path::new(path_str);
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => return KjResult::Err(format!("kj cas put: {}: {}", path_str, e)),
        };

        let mime = mime_from_extension(path_str);
        let cas = self.kernel().cas();

        match cas.store(&data, mime) {
            Ok(hash) => KjResult::Ok {
                message: hash.to_string(),
                content_type: ContentType::Plain,
                ephemeral: false,
            },
            Err(e) => KjResult::Err(format!("kj cas put: {}", e)),
        }
    }

    fn cas_get(&self, argv: &[String]) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err("usage: kj cas get <hash> [--out <path>]".into());
        }

        let hash = match argv[0].parse::<kaijutsu_cas::ContentHash>() {
            Ok(h) => h,
            Err(e) => return KjResult::Err(format!("kj cas get: invalid hash: {}", e)),
        };

        let cas = self.kernel().cas();
        let data = match cas.retrieve(&hash) {
            Ok(Some(d)) => d,
            Ok(None) => return KjResult::Err(format!("kj cas get: not found: {}", hash)),
            Err(e) => return KjResult::Err(format!("kj cas get: {}", e)),
        };

        // --out <path>: write to file
        if argv.len() >= 3 && argv[1] == "--out" {
            match std::fs::write(&argv[2], &data) {
                Ok(()) => {
                    return KjResult::Ok {
                        message: format!("wrote {} bytes to {}", data.len(), argv[2]),
                        content_type: ContentType::Plain,
                        ephemeral: true,
                    }
                }
                Err(e) => return KjResult::Err(format!("kj cas get --out: {}", e)),
            }
        }

        // Default: report size (binary data can't meaningfully go to stdout as text)
        KjResult::Ok {
            message: format!("{} bytes", data.len()),
            content_type: ContentType::Plain,
            ephemeral: true,
        }
    }

    fn cas_ls(&self) -> KjResult {
        let cas = self.kernel().cas();
        let objects_dir = cas.config().objects_dir();

        let mut entries = Vec::new();
        let prefix_dirs = match std::fs::read_dir(&objects_dir) {
            Ok(d) => d,
            Err(_) => {
                return KjResult::Ok {
                    message: "(empty)".into(),
                    content_type: ContentType::Plain,
                    ephemeral: true,
                }
            }
        };

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
                        entries.push(format!("{}  {:>8}  {}", hash, size, mime));
                    }
                }
            }
        }

        entries.sort();
        if entries.is_empty() {
            KjResult::Ok {
                message: "(empty)".into(),
                content_type: ContentType::Plain,
                ephemeral: true,
            }
        } else {
            KjResult::Ok {
                message: entries.join("\n"),
                content_type: ContentType::Plain,
                ephemeral: true,
            }
        }
    }

    fn cas_info(&self, argv: &[String]) -> KjResult {
        let hash_str = match argv.first() {
            Some(h) => h,
            None => return KjResult::Err("usage: kj cas info <hash>".into()),
        };

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
                KjResult::Ok {
                    message: lines.join("\n"),
                    content_type: ContentType::Plain,
                    ephemeral: true,
                }
            }
            Ok(None) => KjResult::Err(format!("kj cas info: not found: {}", hash)),
            Err(e) => KjResult::Err(format!("kj cas info: {}", e)),
        }
    }

    fn cas_rm(&self, argv: &[String]) -> KjResult {
        let hash_str = match argv.first() {
            Some(h) => h,
            None => return KjResult::Err("usage: kj cas rm <hash>".into()),
        };

        let hash = match hash_str.parse::<kaijutsu_cas::ContentHash>() {
            Ok(h) => h,
            Err(e) => return KjResult::Err(format!("kj cas rm: invalid hash: {}", e)),
        };

        let cas = self.kernel().cas();
        match cas.remove(&hash) {
            Ok(true) => KjResult::Ok {
                message: format!("removed {}", hash),
                content_type: ContentType::Plain,
                ephemeral: true,
            },
            Ok(false) => KjResult::Err(format!("kj cas rm: not found: {}", hash)),
            Err(e) => KjResult::Err(format!("kj cas rm: {}", e)),
        }
    }

    fn cas_help(&self) -> String {
        [
            "## kj cas",
            "",
            "Content-addressed storage for binary blobs (images, etc.).",
            "",
            "**Subcommands:**",
            "- `put <path>` — ingest a file, print its hash",
            "- `get <hash> [--out <path>]` — retrieve by hash (write to file with --out)",
            "- `ls` — list all stored objects",
            "- `info <hash>` — show metadata (mime, size, path)",
            "- `rm <hash>` — remove an object (unconditional, no ref-checking)",
        ]
        .join("\n")
    }
}

fn mime_from_extension(path: &str) -> &'static str {
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
