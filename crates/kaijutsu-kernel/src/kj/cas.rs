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
        // `mime_from_extension` only trusts the path's suffix — an image
        // extension is a caller-declared type, not a fact about the bytes.
        // Sniff the first chunk's magic number before committing anything
        // to the streaming writer; a dropped, unfinalized `writer` cleans up
        // its own staging file (see `StreamingWriter`'s `Drop`).
        let needs_image_check = is_supported_image_mime(mime);
        let mut image_checked = !needs_image_check;
        let mut first = true;
        loop {
            let read = if std::mem::take(&mut first) { read_header(&mut file, &mut buf) } else { file.read(&mut buf) };
            let n = match read {
                Ok(n) => n,
                Err(e) => return KjResult::Err(format!("kj cas put: {}: {}", path_str, e)),
            };
            if n == 0 {
                break;
            }
            if !image_checked {
                if let Err(e) = validate_image_bytes(mime, &buf[..n]) {
                    return KjResult::Err(format!("kj cas put: {}: {}", path_str, e));
                }
                image_checked = true;
            }
            if let Err(e) = writer.write(&buf[..n]) {
                return KjResult::Err(format!("kj cas put: {}", e));
            }
        }
        if !image_checked {
            // The loop never ran: an image-declared file with zero bytes
            // can't be a valid image either.
            return KjResult::Err(format!(
                "kj cas put: {}: declared image type '{}' but the file is empty",
                path_str, mime
            ));
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

/// A raster image format this workspace claims to support
/// ([`kaijutsu_types::ContentType::Image`], the `block create --content-type`
/// value list) and can sniff by magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SniffedImageFormat {
    Png,
    Jpeg,
    WebP,
    Gif,
    Avif,
}

impl SniffedImageFormat {
    /// The canonical MIME string this workspace already uses for the format.
    pub fn mime(&self) -> &'static str {
        match self {
            SniffedImageFormat::Png => "image/png",
            SniffedImageFormat::Jpeg => "image/jpeg",
            SniffedImageFormat::WebP => "image/webp",
            SniffedImageFormat::Gif => "image/gif",
            SniffedImageFormat::Avif => "image/avif",
        }
    }
}

/// Sniff `data`'s leading bytes for one of the raster image magic numbers
/// listed in [`SniffedImageFormat`]. `None` when the bytes don't start with
/// any of them, including a slice shorter than the shortest signature.
///
/// Header sniffing only: this never decodes a full image.
pub fn sniff_image_format(data: &[u8]) -> Option<SniffedImageFormat> {
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(SniffedImageFormat::Png);
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(SniffedImageFormat::Jpeg);
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some(SniffedImageFormat::Gif);
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some(SniffedImageFormat::WebP);
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        // An AVIF file may carry a HEIF major brand and name `avif` only
        // among the compatible brands, which follow the 4-byte minor version.
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let end = size.clamp(12, data.len());
        let major = &data[8..12];
        let compatible = data.get(16..end).unwrap_or(&[]).chunks_exact(4);
        if std::iter::once(major).chain(compatible).any(|brand| matches!(brand, b"avif" | b"avis")) {
            return Some(SniffedImageFormat::Avif);
        }
    }
    None
}

/// True when `mime` is one of the raster image types [`sniff_image_format`]
/// recognizes.
pub fn is_supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "image/avif"
    )
}

/// Refuse an image import whose declared MIME type disagrees with its
/// actual bytes. `declared_mime` is what the caller claims (a file
/// extension, or an explicit `--content-type`); `data` is sniffed by magic
/// number only, never fully decoded. A mismatch, and bytes that don't sniff
/// as any supported raster format, are both loud errors naming the declared
/// type and, when one was found, the detected type — no silent fallback and
/// no re-labeling to the sniffed type.
pub fn validate_image_bytes(declared_mime: &str, data: &[u8]) -> Result<(), String> {
    if declared_mime == "image/svg+xml" {
        // SVG has no magic number: accept XML text whose opening names an
        // `<svg` element.
        let head = String::from_utf8_lossy(&data[..data.len().min(1024)]);
        return if head.trim_start_matches('\u{feff}').trim_start().starts_with('<') && head.contains("<svg") {
            Ok(())
        } else {
            Err(format!("declared image type '{declared_mime}' but the bytes do not open an <svg> element"))
        };
    }
    match sniff_image_format(data) {
        Some(fmt) if fmt.mime() == declared_mime => Ok(()),
        Some(fmt) => Err(format!(
            "declared image type '{declared_mime}' but the bytes' magic number is '{}'",
            fmt.mime()
        )),
        None => Err(format!(
            "declared image type '{declared_mime}' but the bytes are not a recognized image \
             format (checked PNG, JPEG, GIF, WebP, AVIF magic numbers)"
        )),
    }
}

/// Bytes a caller should offer [`sniff_image_format`]: enough for an `ftyp`
/// box with a long compatible-brands list.
pub const SNIFF_LEN: usize = 64;

/// Read until `buf` holds at least [`SNIFF_LEN`] bytes or the reader ends.
fn read_header(reader: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < SNIFF_LEN.min(buf.len()) {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
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

// Verb class: kj/effect.rs
use super::effect::{Classify, Effect};

impl Classify for CasArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for CasCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::Ls | Self::Info { .. } => Effect::Read,
            Self::Get { out: None, .. } => Effect::Read,
            Self::Get { out: Some(_), .. } => Effect::Write,
            Self::Put { .. } | Self::Rm { .. } => Effect::Write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sniff_image_format, validate_image_bytes, SniffedImageFormat};

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = (16 + 4 * compatible.len()) as u32;
        let mut data = size.to_be_bytes().to_vec();
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(major);
        data.extend_from_slice(&[0, 0, 0, 0]);
        for brand in compatible { data.extend_from_slice(*brand); }
        data.extend_from_slice(b"\0\0\0\x08mdat");
        data
    }

    #[test]
    fn validate_accepts_svg_text_and_refuses_other_text_named_svg() {
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
        assert!(validate_image_bytes("image/svg+xml", svg).is_ok());
        assert!(validate_image_bytes("image/svg+xml", b"plain text").is_err());
        assert!(validate_image_bytes("image/svg+xml", REAL_PNG).is_err());
    }

    #[test]
    fn sniff_accepts_avif_named_only_in_the_compatible_brands() {
        let data = ftyp(b"mif1", &[b"mif1", b"avif", b"miaf"]);
        assert_eq!(sniff_image_format(&data), Some(SniffedImageFormat::Avif));
    }

    #[test]
    fn sniff_refuses_a_heif_container_without_an_avif_brand() {
        let data = ftyp(b"mif1", &[b"mif1", b"heic"]);
        assert_eq!(sniff_image_format(&data), None);
    }

    /// A pipe or device may return a few bytes per read; the sniff needs the
    /// header whole.
    #[test]
    fn read_header_fills_across_short_reads() {
        struct Trickle<'a>(&'a [u8]);
        impl std::io::Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(3).min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let mut buf = vec![0u8; 4096];
        let n = super::read_header(&mut Trickle(REAL_PNG), &mut buf).unwrap();
        assert!(n >= super::SNIFF_LEN.min(REAL_PNG.len()), "read {n} bytes");
        assert_eq!(sniff_image_format(&buf[..n]), Some(SniffedImageFormat::Png));
    }

    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use kaijutsu_cas::ContentStore;
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

    // ── Image byte sniffing (docs/issues.md, "block-tool contracts") ────
    //
    // A 1x1 real PNG and a 1x1 real JPEG (generated once via Pillow), kept
    // as raw bytes so these tests exercise the same magic-number path a
    // real upload would.
    const REAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
        0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
        0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    const REAL_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
        0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43,
        0x00, 0x10, 0x0B, 0x0C, 0x0E, 0x0C, 0x0A, 0x10, 0x0E, 0x0D, 0x0E, 0x12,
        0x11, 0x10, 0x13, 0x18, 0x28, 0x1A, 0x18, 0x16, 0x16, 0x18, 0x31, 0x23,
        0x25, 0x1D, 0x28, 0x3A, 0x33, 0x3D, 0x3C, 0x39, 0x33, 0x38, 0x37, 0x40,
        0x48, 0x5C, 0x4E, 0x40, 0x44, 0x57, 0x45, 0x37, 0x38, 0x50, 0x6D, 0x51,
        0x57, 0x5F, 0x62, 0x67, 0x68, 0x67, 0x3E, 0x4D, 0x71, 0x79, 0x70, 0x64,
        0x78, 0x5C, 0x65, 0x67, 0x63, 0xFF, 0xDB, 0x00, 0x43, 0x01, 0x11, 0x12,
        0x12, 0x18, 0x15, 0x18, 0x2F, 0x1A, 0x1A, 0x2F, 0x63, 0x42, 0x38, 0x42,
        0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
        0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
        0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
        0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
        0x63, 0x63, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01, 0x03,
        0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xC4, 0x00,
        0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
        0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00,
        0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00,
        0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81,
        0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24,
        0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A,
        0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A,
        0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86,
        0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3,
        0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6,
        0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9,
        0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1,
        0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xC4, 0x00,
        0x1F, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
        0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x11, 0x00,
        0x02, 0x01, 0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00,
        0x01, 0x02, 0x77, 0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31,
        0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08,
        0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0, 0x15,
        0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18,
        0x19, 0x1A, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39,
        0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55,
        0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
        0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84,
        0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA,
        0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4,
        0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7,
        0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
        0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00,
        0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3F, 0x00, 0xC5,
        0xA2, 0x8A, 0x2B, 0xCB, 0x3E, 0xF0, 0xFF, 0xD9,
    ];

    #[test]
    fn sniff_image_format_recognizes_declared_formats() {
        assert_eq!(sniff_image_format(REAL_PNG), Some(SniffedImageFormat::Png));
        assert_eq!(sniff_image_format(REAL_JPEG), Some(SniffedImageFormat::Jpeg));
        assert_eq!(sniff_image_format(b"not an image at all"), None);
        assert_eq!(sniff_image_format(b""), None);
    }

    #[test]
    fn validate_image_bytes_accepts_matching_real_images() {
        assert!(validate_image_bytes("image/png", REAL_PNG).is_ok());
        assert!(validate_image_bytes("image/jpeg", REAL_JPEG).is_ok());
    }

    /// Regression (a): PNG bytes declared/named as JPEG must be refused, and
    /// the error must name both the declared and the detected type.
    #[test]
    fn validate_image_bytes_refuses_png_declared_as_jpeg() {
        let err = validate_image_bytes("image/jpeg", REAL_PNG)
            .expect_err("PNG bytes declared as JPEG must be refused");
        assert!(err.contains("image/jpeg"), "must name the declared type: {err}");
        assert!(err.contains("image/png"), "must name the detected type: {err}");
    }

    /// Regression (b): non-image bytes named as an image type must be
    /// refused, not silently accepted or re-labeled.
    #[test]
    fn validate_image_bytes_refuses_text_declared_as_png() {
        let err = validate_image_bytes("image/png", b"just some plain text, not an image")
            .expect_err("text bytes declared as PNG must be refused");
        assert!(err.contains("image/png"), "must name the declared type: {err}");
    }

    /// Regression (c): genuine PNG and JPEG bytes, correctly declared, are
    /// accepted — companion to (a)/(b) so the check isn't just refusing
    /// everything.
    #[test]
    fn validate_image_bytes_accepts_genuine_png_and_jpeg() {
        assert!(validate_image_bytes("image/png", REAL_PNG).is_ok());
        assert!(validate_image_bytes("image/jpeg", REAL_JPEG).is_ok());
    }

    /// `kj cas put` end-to-end: a file with a `.jpg` extension but real PNG
    /// bytes must be refused before anything lands in the CAS.
    #[tokio::test]
    async fn cas_put_refuses_extension_mismatched_bytes() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let dir = tempfile::tempdir().expect("tmpdir");
        let src = dir.path().join("mislabeled.jpg");
        std::fs::write(&src, REAL_PNG).expect("write src");

        let res = dispatcher.dispatch_cas(
            &["put".to_string(), src.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(!res.is_ok(), "extension/bytes mismatch must be refused: {res:?}");
        assert!(res.message().contains("image/jpeg"), "msg: {}", res.message());
        assert!(res.message().contains("image/png"), "msg: {}", res.message());

        // Nothing should have landed in the CAS.
        let hash = kaijutsu_cas::ContentHash::from_data(REAL_PNG);
        assert!(
            dispatcher.kernel().cas().retrieve(&hash).unwrap().is_none(),
            "rejected bytes must not be stored"
        );
    }

    /// `kj cas put` end-to-end: text content named with an image extension
    /// must be refused.
    #[tokio::test]
    async fn cas_put_refuses_text_named_as_image() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let dir = tempfile::tempdir().expect("tmpdir");
        let src = dir.path().join("not-a-real-image.png");
        std::fs::write(&src, b"just some plain text, not an image").expect("write src");

        let res = dispatcher.dispatch_cas(
            &["put".to_string(), src.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(!res.is_ok(), "text named as .png must be refused: {res:?}");
        assert!(res.message().contains("image/png"), "msg: {}", res.message());
    }

    /// `kj cas put` end-to-end: genuine PNG and JPEG files, correctly named,
    /// are accepted and land in the CAS under their declared mime.
    #[tokio::test]
    async fn cas_put_accepts_genuine_png_and_jpeg() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();
        let dir = tempfile::tempdir().expect("tmpdir");

        let png_src = dir.path().join("real.png");
        std::fs::write(&png_src, REAL_PNG).expect("write png");
        let res = dispatcher.dispatch_cas(
            &["put".to_string(), png_src.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(res.is_ok(), "genuine PNG put failed: {}", res.message());
        let hash: kaijutsu_cas::ContentHash = res.message().trim().parse().expect("valid hash");
        let info = dispatcher.kernel().cas().inspect(&hash).unwrap().expect("stored");
        assert_eq!(info.mime_type, "image/png");

        let jpeg_src = dir.path().join("real.jpg");
        std::fs::write(&jpeg_src, REAL_JPEG).expect("write jpeg");
        let res = dispatcher.dispatch_cas(
            &["put".to_string(), jpeg_src.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(res.is_ok(), "genuine JPEG put failed: {}", res.message());
        let hash: kaijutsu_cas::ContentHash = res.message().trim().parse().expect("valid hash");
        let info = dispatcher.kernel().cas().inspect(&hash).unwrap().expect("stored");
        assert_eq!(info.mime_type, "image/jpeg");
    }

    /// Non-image `kj cas put` traffic is untouched by the new check — an
    /// arbitrary binary blob with no image extension still round-trips.
    #[tokio::test]
    async fn cas_put_non_image_extension_is_unaffected() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let dir = tempfile::tempdir().expect("tmpdir");
        let src = dir.path().join("blob.bin");
        std::fs::write(&src, b"arbitrary non-image bytes").expect("write src");
        let res = dispatcher.dispatch_cas(
            &["put".to_string(), src.to_string_lossy().into_owned()],
            &caller,
        );
        assert!(res.is_ok(), "non-image put must be unaffected: {}", res.message());
    }
}
