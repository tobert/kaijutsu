//! Provider request sensors: an always-on summary line and an opt-in dump.
//!
//! Every provider that builds a request body calls [`record`] once per send
//! attempt, after the endpoint slot is held and just before the send, so a
//! request that never got a slot leaves nothing. `record` logs one `info` line
//! with counts and a hash (never message text). When the trace carries a dump
//! directory it also hands a file write to a detached blocking task: the
//! request never waits for it.
//!
//! The dump is off unless `KJ_LLM_REQUEST_DUMP=<dir>` is set in the server's
//! environment; `llm_stream` reads it once per turn and puts the result in
//! [`RequestTrace::dump_dir`]. The file holds the body value in a header
//! envelope, pretty-printed, with image payloads replaced by markers. Files
//! are written whole (temp file, then rename) with mode 0600; directories this
//! module creates are mode 0700. Neither sensor can fail a request: a write
//! error logs one warning per turn and the request proceeds. See
//! `docs/telemetry.md`, "Provider request dump".

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kaijutsu_types::{ContextId, TurnId};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::llm::{ContentBlock, Message, MessageContent, Role};

/// Environment variable naming the dump directory.
pub const DUMP_ENV: &str = "KJ_LLM_REQUEST_DUMP";

/// Which send attempt a request is. Carried on `BuildOpts`.
#[derive(Debug, Clone)]
pub struct RequestTrace {
    pub context: ContextId,
    pub turn: TurnId,
    /// Model call number within the turn, from 1 (one per agentic-loop round).
    pub step: u32,
    /// Attempt number within the step, from 1; the turn's startup retry loop
    /// raises it.
    pub attempt: u32,
    /// Where to write dump files; `None` writes nothing.
    pub dump_dir: Option<PathBuf>,
    /// Set once a write failure has been logged for this turn; shared by every
    /// step's clone.
    warned: Arc<AtomicBool>,
}

impl RequestTrace {
    pub fn new(context: ContextId, turn: TurnId, step: u32, dump_dir: Option<PathBuf>) -> Self {
        Self { context, turn, step, attempt: 1, dump_dir, warned: Arc::new(AtomicBool::new(false)) }
    }

    /// The same trace with the dump turned off, for a provider whose request
    /// is not a serializable body.
    pub fn summary_only(&self) -> Self {
        Self { dump_dir: None, ..self.clone() }
    }

    /// The same turn at another step, on its first attempt.
    pub fn at_step(&self, step: u32) -> Self {
        Self { step, attempt: 1, ..self.clone() }
    }

    /// The same step at another attempt.
    pub fn at_attempt(&self, attempt: u32) -> Self {
        Self { attempt, ..self.clone() }
    }
}

/// The dump directory named by [`DUMP_ENV`], if set and non-empty.
pub fn dump_dir_from_env() -> Option<PathBuf> {
    std::env::var_os(DUMP_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The last user message that carries text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastUser {
    /// Index into the message list.
    pub index: usize,
    pub chars: usize,
    /// First 8 hex digits of the SHA-256 of the message text.
    pub hash: String,
}

/// Counts that identify a request without its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSummary {
    pub messages: usize,
    /// Characters of system prompt, message text, tool input and tool output.
    pub total_chars: usize,
    pub last_user: Option<LastUser>,
}

/// Text of a message: text blocks only, joined as stored.
fn user_text(m: &Message) -> String {
    match &m.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(bs) => bs
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn block_chars(b: &ContentBlock) -> usize {
    match b {
        ContentBlock::Text { text } => text.chars().count(),
        ContentBlock::ToolUse { input, .. } => input.to_string().chars().count(),
        ContentBlock::ToolResult { content, .. } => content.chars().count(),
        ContentBlock::Reasoning { text, .. } => text.chars().count(),
        ContentBlock::Image { .. } => 0,
    }
}

pub fn hash8(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn summarize(system: Option<&str>, messages: &[Message]) -> RequestSummary {
    let mut total = system.map_or(0, |s| s.chars().count());
    for m in messages {
        total += match &m.content {
            MessageContent::Text(t) => t.chars().count(),
            MessageContent::Blocks(bs) => bs.iter().map(block_chars).sum(),
        };
    }
    let last_user = messages.iter().enumerate().rev().find_map(|(index, m)| {
        if m.role != Role::User {
            return None;
        }
        let text = user_text(m);
        (!text.is_empty()).then(|| LastUser { index, chars: text.chars().count(), hash: hash8(&text) })
    });
    RequestSummary { messages: messages.len(), total_chars: total, last_user }
}

#[derive(Serialize)]
struct Header<'a> {
    context: String,
    turn: String,
    step: u32,
    attempt: u32,
    provider: &'a str,
    model: &'a str,
    endpoint_host: &'a str,
    summary: SummaryJson,
}

#[derive(Serialize)]
struct SummaryJson {
    messages: usize,
    total_chars: usize,
    last_user_chars: Option<usize>,
    last_user_hash: Option<String>,
}

/// `YYYYMMDDTHHMMSS.mmmZ` for a Unix time in milliseconds.
fn utc_stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

fn file_name(ms: u64, t: &RequestTrace, provider: &str) -> String {
    let provider: String = provider
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{}-{}-{}-s{:03}-a{}-{}.json",
        utc_stamp(ms),
        t.context.short(),
        t.turn.short(),
        t.step,
        t.attempt,
        provider
    )
}

/// Write `bytes` as `dir/name` whole: a private temp file, then a rename, so a
/// full disk leaves no truncated file under the final name.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(path)
}

/// Replace base64 image payloads in a dump copy with markers naming media
/// type, size and hash. Covers Anthropic `source.data` and `data:` URLs.
fn elide_images(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let media = map
                .get("media_type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let is_b64 = map.get("type").and_then(Value::as_str) == Some("base64");
            if let (true, Some(Value::String(data))) = (is_b64, map.get_mut("data")) {
                *data = marker(media.as_deref().unwrap_or("unknown"), data);
            }
            for child in map.values_mut() {
                elide_images(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(elide_images),
        Value::String(s) => {
            if let Some(rest) = s.strip_prefix("data:")
                && let Some((media, payload)) = rest.split_once(";base64,")
            {
                *s = marker(media, payload);
            }
        }
        _ => {}
    }
}

fn marker(media_type: &str, base64: &str) -> String {
    format!(
        "<image elided: media_type={media_type} base64_chars={} sha256_8={}>",
        base64.chars().count(),
        hash8(base64)
    )
}

/// Log the request summary and, when the trace names a dump directory, write
/// the body in the background. `body` is the value serialized onto the wire;
/// the file holds a pretty-printed copy of it with images elided. Never fails.
///
/// Returns the writer task when one was spawned (tests await it); the request
/// path ignores it.
pub fn record<B: Serialize>(
    trace: Option<&RequestTrace>,
    provider: &str,
    model: &str,
    endpoint_url: &str,
    system: Option<&str>,
    messages: &[Message],
    body: &B,
) -> Option<tokio::task::JoinHandle<()>> {
    let s = summarize(system, messages);
    let (last_chars, last_hash) = match &s.last_user {
        Some(l) => (Some(l.chars), Some(l.hash.clone())),
        None => (None, None),
    };
    let ctx = trace.map(|t| t.context.short());
    let turn = trace.map(|t| t.turn.short());
    tracing::info!(
        context = ctx.as_deref(),
        turn = turn.as_deref(),
        step = trace.map(|t| t.step),
        attempt = trace.map(|t| t.attempt),
        provider,
        model,
        messages = s.messages,
        total_chars = s.total_chars,
        last_user_chars = last_chars,
        last_user_hash = last_hash.as_deref(),
        "llm request"
    );

    let t = trace?;
    let dir = t.dump_dir.clone()?;
    let warn = {
        let t = t.clone();
        let dir = dir.clone();
        move |e: &dyn std::fmt::Display| {
            if !t.warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(dir = %dir.display(), error = %e, "{DUMP_ENV}: request dump failed; the request proceeds");
            }
        }
    };
    let mut body = match serde_json::to_value(body) {
        Ok(v) => v,
        Err(e) => {
            warn(&e);
            return None;
        }
    };
    let host = reqwest::Url::parse(endpoint_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let (provider, model, t) = (provider.to_owned(), model.to_owned(), t.clone());
    let work = move || {
        elide_images(&mut body);
        let header = Header {
            context: t.context.to_hex(),
            turn: t.turn.to_hex(),
            step: t.step,
            attempt: t.attempt,
            provider: &provider,
            model: &model,
            endpoint_host: &host,
            summary: SummaryJson {
                messages: s.messages,
                total_chars: s.total_chars,
                last_user_chars: last_chars,
                last_user_hash: last_hash,
            },
        };
        let result = serde_json::to_vec_pretty(&serde_json::json!({ "header": header, "body": body }))
            .map_err(std::io::Error::other)
            .and_then(|bytes| write_file(&dir, &file_name(kaijutsu_types::now_millis(), &t, &provider), &bytes));
        if let Err(e) = result {
            warn(&e);
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Some(handle.spawn_blocking(work)),
        Err(_) => {
            work();
            None
        }
    }
}


/// Wait for `n` finished dump files in `dir`; for tests that cannot hold the
/// writer task's handle.
#[cfg(test)]
pub(crate) async fn wait_for_files(dir: &Path, n: usize) -> Vec<PathBuf> {
    for _ in 0..200 {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .map(|d| d.map(|e| e.unwrap().path()).filter(|p| !p.file_name().unwrap().to_string_lossy().ends_with(".tmp")).collect())
            .unwrap_or_default();
        if v.len() >= n {
            v.sort();
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no {n} dump file(s) in {}", dir.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    fn trace(dir: Option<PathBuf>) -> RequestTrace {
        RequestTrace::new(ContextId::new(), TurnId::new(), 3, dir)
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        v.sort();
        v
    }

    #[test]
    fn the_dump_holds_the_exact_body_and_no_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let t = trace(Some(tmp.path().join("nested/dumps")));
        let body = json!({"model": "m", "messages": [{"role": "user", "content": "let's do a thing"}]});
        let msgs = vec![Message::user("let's do a thing")];
        // The key rides only in the endpoint userinfo and a header-ish URL
        // query: neither may reach the file.
        record(Some(&t), "alibaba", "m", "https://user:sk-SECRET-123@dashscope.example/v1?key=sk-SECRET-123", None, &msgs, &body);
        let f = files(&tmp.path().join("nested/dumps"));
        assert_eq!(f.len(), 1);
        let text = std::fs::read_to_string(&f[0]).unwrap();
        assert!(!text.contains("sk-SECRET-123"), "secret leaked: {text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["body"], body);
        let h = &v["header"];
        assert_eq!(h["context"], t.context.to_hex());
        assert_eq!(h["turn"], t.turn.to_hex());
        assert_eq!(h["step"], 3);
        assert_eq!(h["provider"], "alibaba");
        assert_eq!(h["model"], "m");
        assert_eq!(h["endpoint_host"], "dashscope.example");
        assert_eq!(h["summary"]["last_user_chars"], 16);
        let keys: Vec<_> = h.as_object().unwrap().keys().cloned().collect();
        assert!(!keys.iter().any(|k| k.contains("auth") || k.contains("key")), "{keys:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_dump_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        record(Some(&trace(Some(tmp.path().to_path_buf()))), "p", "m", "http://h/", None, &[], &json!({}));
        let mode = std::fs::metadata(&files(tmp.path())[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn file_names_sort_by_time_and_name_their_request() {
        let t = trace(None);
        let a = file_name(1_760_000_000_123, &t, "open/ai");
        let b = file_name(1_760_000_000_124, &t, "open/ai");
        assert!(a < b);
        assert_eq!(
            a,
            format!("20251009T085320.123Z-{}-{}-s003-a1-open_ai.json", t.context.short(), t.turn.short())
        );
        assert_eq!(utc_stamp(0), "19700101T000000.000Z");
        assert_eq!(utc_stamp(951_782_400_000), "20000229T000000.000Z");
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_dump_dir_does_not_fail_the_request() {
        // A regular file where the directory should be.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let t = trace(Some(blocker.join("sub")));
        record(Some(&t), "p", "m", "http://h/", None, &[], &json!({}));
        assert!(t.warned.load(Ordering::Relaxed), "the failure is reported");
        // A second step of the same turn does not warn again: the flag is shared.
        assert!(t.at_step(4).warned.load(Ordering::Relaxed));
        record(Some(&t.at_step(4)), "p", "m", "http://h/", None, &[], &json!({}));
    }

    #[test]
    fn summary_counts_chars_not_bytes_and_hashes_the_last_user_text() {
        let msgs = vec![
            Message::user("first"),
            Message::assistant("ok"),
            Message::user("let's do a 日本語"),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t".into(),
                content: "abc".into(),
                is_error: false,
            }]),
        ];
        let s = summarize(Some("sys"), &msgs);
        assert_eq!(s.messages, 4);
        assert_eq!(s.total_chars, 3 + 5 + 2 + 14 + 3);
        // The tool-result-only message has no text; the last user text is #2.
        let last = s.last_user.unwrap();
        assert_eq!((last.index, last.chars), (2, 14));
        assert_eq!(last.hash, hash8("let's do a 日本語"));
        // SHA-256("abc") starts ba7816bf: pins the hash function.
        assert_eq!(hash8("abc"), "ba7816bf");
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    #[test]
    fn the_info_line_carries_counts_and_hash_but_no_text() {
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).finish();
        let msgs = vec![Message::user("let's do a thing")];
        tracing::subscriber::with_default(sub, || {
            record(Some(&trace(None)), "alibaba", "qwen", "http://h/", None, &msgs, &json!({}));
        });
        let line = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(line.contains("llm request"), "{line}");
        assert!(line.contains("last_user_chars=16"), "{line}");
        assert!(line.contains(&format!("last_user_hash=\"{}\"", hash8("let's do a thing"))), "{line}");
        assert!(line.contains("total_chars=16") && line.contains("messages=1"), "{line}");
        assert!(!line.contains("let's do a"), "message text leaked: {line}");
    }

    /// `BuildOpts` sits by value in every provider stream future, and
    /// `kj::context::tests::a_root_cwd_is_accepted_with_a_warning` runs near
    /// the 2 MB test-thread stack limit: growing it by the trace's ~80 bytes
    /// overflowed that test. The trace must stay one pointer.
    #[test]
    fn the_trace_on_build_opts_stays_pointer_sized() {
        use crate::llm::stream::BuildOpts;
        let field = std::mem::size_of::<Option<Arc<RequestTrace>>>();
        assert_eq!(field, std::mem::size_of::<usize>());
        let opts: BuildOpts = BuildOpts::new("m", 4096);
        assert_eq!(std::mem::size_of_val(&opts.trace), field);
    }

    #[test]
    fn images_are_elided_in_the_dump_copy_only() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "QUJD".repeat(50_000);
        let body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}},
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{big}")}},
                {"type": "text", "text": "keep me"}
            ]}
        ]});
        let before = body.to_string();
        record(Some(&trace(Some(tmp.path().to_path_buf()))), "p", "m", "http://h/", None, &[], &body);
        assert_eq!(body.to_string(), before, "the wire value is untouched");
        let text = std::fs::read_to_string(&files(tmp.path())[0]).unwrap();
        assert!(text.len() < 2_000, "payload leaked: {} bytes", text.len());
        let v: Value = serde_json::from_str(&text).unwrap();
        let parts = &v["body"]["messages"][0]["content"];
        let want = format!("<image elided: media_type=image/png base64_chars=200000 sha256_8={}>", hash8(&big));
        assert_eq!(parts[0]["source"]["data"], want);
        assert_eq!(parts[0]["source"]["media_type"], "image/png");
        assert_eq!(
            parts[1]["image_url"]["url"],
            format!("<image elided: media_type=image/jpeg base64_chars=200000 sha256_8={}>", hash8(&big))
        );
        assert_eq!(parts[2]["text"], "keep me");
    }

    #[test]
    fn a_retry_gets_its_own_file_and_line() {
        let tmp = tempfile::tempdir().unwrap();
        let t = trace(Some(tmp.path().to_path_buf()));
        record(Some(&t.at_attempt(1)), "p", "m", "http://h/", None, &[], &json!({}));
        record(Some(&t.at_attempt(2)), "p", "m", "http://h/", None, &[], &json!({}));
        let names: Vec<String> =
            files(tmp.path()).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().any(|n| n.contains("-s003-a1-")), "{names:?}");
        assert!(names.iter().any(|n| n.contains("-s003-a2-")), "{names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".tmp")));
        let v: Value = serde_json::from_slice(&std::fs::read(&files(tmp.path())[1]).unwrap()).unwrap();
        assert!(v["header"]["attempt"] == 1 || v["header"]["attempt"] == 2);
        // A new step starts again at attempt 1.
        assert_eq!(t.at_attempt(2).at_step(4).attempt, 1);
    }

    #[test]
    fn the_info_line_names_the_attempt() {
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).finish();
        tracing::subscriber::with_default(sub, || {
            record(Some(&trace(None).at_attempt(2)), "p", "m", "http://h/", None, &[], &json!({}));
        });
        let line = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(line.contains("attempt=2") && line.contains("step=3"), "{line}");
    }

    #[cfg(unix)]
    #[test]
    fn the_dir_is_0700_and_a_failed_write_leaves_no_partial_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        write_file(&dir, "x.json", b"{}").unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        // The final name exists already: the rename target is a directory.
        std::fs::create_dir(dir.join("y.json")).unwrap();
        assert!(write_file(&dir, "y.json", b"{}").is_err());
        let left: Vec<_> = files(&dir).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(!left.iter().any(|n| n.ends_with(".tmp")), "{left:?}");
    }

    /// Under a runtime the file is written by a detached blocking task.
    #[tokio::test]
    async fn under_a_runtime_the_write_is_a_background_task() {
        let tmp = tempfile::tempdir().unwrap();
        let h = record(Some(&trace(Some(tmp.path().to_path_buf()))), "p", "m", "http://h/", None, &[], &json!({"a": 1}));
        h.expect("a task was spawned").await.unwrap();
        assert_eq!(files(tmp.path()).len(), 1);
    }
}
