//! The public shell result shared by runtime and transport projections.
//!
//! Every field is always present. An unavailable value is `null`, which is
//! distinct from zero, false, and an empty string. `is_error` follows `status`;
//! command exits, interpreter rejection, hook replacements, and incomplete
//! operations retain their distinct meanings. See `docs/shell-envelope.md`.

use serde::{Deserialize, Serialize};

/// How a `shell` call ended. Finer than `is_error`, which collapses every
/// non-success onto one bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStatus {
    /// The command exited 0, or a hook supplied a successful replacement.
    Done,
    /// Nonzero exit, execution/persistence failure, or an error hook result.
    Error,
    /// kaish refused the program before running it — a parse or validation
    /// failure. Nothing ran, and the text is the caller's to fix.
    Rejected,
    /// A background command was started and is still running. Its output
    /// is available through `read_shell_operation` and `kj wait --operation`.
    Running,
    /// Accepted work waiting for an approval decision.
    Waiting,
    /// The call gave up waiting for the command's outcome.
    Timeout,
    /// The event stream closed before the command's outcome arrived.
    StreamClosed,
}

impl ShellStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ShellStatus::Done => "done",
            ShellStatus::Error => "error",
            ShellStatus::Rejected => "rejected",
            ShellStatus::Running => "running",
            ShellStatus::Waiting => "waiting",
            ShellStatus::Timeout => "timeout",
            ShellStatus::StreamClosed => "stream_closed",
        }
    }

    /// Whether this status rides the tool-result error flag. `Done` and
    /// `Running` do not; everything else does.
    pub fn is_error(&self) -> bool {
        !matches!(self, ShellStatus::Done | ShellStatus::Running | ShellStatus::Waiting)
    }
}

/// The `shell` result envelope. Serializes with every key present.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShellEnvelope {
    /// The command's standard output. Empty string when it wrote none.
    pub stdout: String,
    /// The command's standard error, kept separate from `stdout`. Empty
    /// string when it wrote none.
    pub stderr: String,
    /// The command's exit code. `null` when no code exists to report: the
    /// program was refused, a hook supplied a replacement, or the code is
    /// unavailable to this caller. `null` is never evidence of success.
    pub exit_code: Option<i64>,
    /// How the call ended.
    pub status: ShellStatus,
    /// Whether output was capped and the tail dropped. `null` when the
    /// serving path cannot tell.
    pub did_spill: Option<bool>,
    /// A `kj` verb's structured payload — arrays for list verbs, objects for
    /// inspect. `null` when the command set none.
    pub data: Option<serde_json::Value>,
    /// A `kj` confirmation gate's re-run hint (`command`, `target`, `hint`).
    /// `null` when the command did not latch.
    pub latch: Option<serde_json::Value>,
    /// The block the command's output landed in. `null` when the serving
    /// path has no block to name.
    pub block_id: Option<String>,
    /// The durable command handle, retained while waiting and after completion.
    /// `null` when no operation was registered.
    pub operation_id: Option<String>,
    /// The decision this operation is waiting for, when present.
    pub ask_id: Option<String>,
    /// MIME type of the result block's content. `null` when unknown.
    pub content_type: Option<String>,
    /// Whether the result block is excluded from model hydration. `null`
    /// when unknown.
    pub ephemeral: Option<bool>,
    /// Wall time the call spent, in milliseconds. `null` when unmeasured.
    pub elapsed_ms: Option<u64>,
    /// Why the command's outcome never arrived, or why it was refused.
    /// `null` when the command produced a real outcome.
    pub error: Option<String>,
    /// The shell's working directory after this call, when the call changed
    /// it. `null` when it did not, or when the serving path cannot tell.
    pub cwd: Option<String>,
    /// A condition the caller should correct before it goes on, such as a
    /// shell left at `/`. `null` when there is none, or when the serving
    /// path cannot tell.
    pub warning: Option<String>,
}

/// Whether `cwd` names the filesystem root.
///
/// kaish accepts `/` as a working directory. A kaijutsu seat at `/` is almost
/// always a mistake, so every surface that leaves a shell there says so and
/// none refuses it. See `docs/shell-envelope.md`, "A cwd of `/`".
pub fn is_root_cwd(cwd: &std::path::Path) -> bool {
    cwd.has_root() && cwd.parent().is_none()
}

/// The shell result's warning for a shell whose cwd is `cwd`: present only
/// for `/`.
pub fn root_cwd_warning(cwd: &std::path::Path) -> Option<String> {
    is_root_cwd(cwd).then(|| "cwd is /, which is almost always a mistake; cd to your work tree".to_string())
}

/// The line that marks where stderr starts in a joined result.
pub const STDERR_MARKER: &str = "[stderr]";

/// stdout, then a `[stderr]` line and stderr when stderr is not empty. Every
/// path that shows a model or a person both streams as one text joins them
/// here, so a reader can always tell which lines came from stderr.
pub fn join_streams(stdout: &str, stderr: &str) -> String {
    let mut out = String::with_capacity(stdout.len() + stderr.len() + STDERR_MARKER.len() + 2);
    out.push_str(stdout);
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(STDERR_MARKER);
        out.push('\n');
        out.push_str(stderr);
    }
    out
}

impl ShellEnvelope {
    /// An envelope with every optional field unset. Fill in what the serving
    /// path actually knows and leave the rest `null`.
    pub fn new(status: ShellStatus) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            status,
            did_spill: None,
            data: None,
            latch: None,
            block_id: None,
            operation_id: None,
            ask_id: None,
            content_type: None,
            ephemeral: None,
            elapsed_ms: None,
            error: None,
            cwd: None,
            warning: None,
        }
    }

    /// The status an exit code implies: 0 is `Done`, anything else `Error`.
    pub fn status_for_exit(code: i64) -> ShellStatus {
        if code == 0 {
            ShellStatus::Done
        } else {
            ShellStatus::Error
        }
    }

    /// Whether this result rides the tool-result error flag.
    pub fn is_error(&self) -> bool {
        self.status.is_error()
    }

    /// The envelope as JSON. Infallible: every field is a plain scalar,
    /// string, or already-built `serde_json::Value`.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ShellEnvelope contains no non-serializable field")
    }

    /// Recover an envelope from a tool result's text body, or `None` when the
    /// body is not one.
    ///
    /// Deserialization is the test, not a guess about the tool's name:
    /// `stdout`, `stderr`, and `status` are required, so a body that is not a
    /// shell envelope cannot be mistaken for one. An absent optional key
    /// reads as `null`, so an envelope stored before a key existed still
    /// reads. This is
    /// what lets a consumer holding only the flattened text — the agentic
    /// loop's tool dispatch — tell an envelope from ordinary tool output.
    pub fn from_tool_result(body: &str) -> Option<Self> {
        let trimmed = body.trim_start();
        if !trimmed.starts_with('{') {
            return None;
        }
        serde_json::from_str(trimmed).ok()
    }

    /// What a person reads: the command's output, without the envelope around
    /// it. stdout comes first; when the command wrote to stderr, a `[stderr]`
    /// line follows, then stderr. A refusal's reason, which rides in `error`,
    /// reads as stderr. See `join_streams`.
    ///
    /// This is the durable block's text. The envelope is for the model; a
    /// block is read by people and replayed by hydration, and a JSON object
    /// serves neither.
    pub fn readable_output(&self) -> String {
        let mut stderr = self.stderr.clone();
        if let Some(err) = &self.error {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(err);
        }
        join_streams(&self.stdout, &stderr)
    }

    /// This envelope carrying `clean` as its output, so the model and the
    /// durable block hold one text with one set of byte offsets.
    ///
    /// `clean` is `readable_output` after ANSI stripping, so it is the
    /// already-joined pair, `[stderr]` marker included. It replaces `stdout`; `stderr` and `error` are
    /// emptied rather than left to repeat text that is now inside `stdout`.
    /// `exit_code`, `status` and every other field are untouched — the
    /// distinction between "wrote to stderr" and "exited nonzero" lives in
    /// `status` and `exit_code`, which is where a caller was told to read it.
    pub fn with_clean_output(mut self, clean: &str) -> Self {
        self.stdout = clean.to_string();
        self.stderr = String::new();
        self.error = None;
        self
    }

    /// What a model reads for this result in a kernel turn: `clean` (the
    /// readable output, as the durable block stores it), then one bracketed
    /// line for each fact that changes what the model should do next. A clean
    /// success is its output alone. Always text, never JSON, and never empty:
    /// a command that printed nothing reads `[no output]`, so every result has
    /// one shape. See `docs/shell-envelope.md`, "What a model turn reads".
    pub fn model_text(&self, clean: &str) -> String {
        let operation = |label: &str| match (&self.operation_id, &self.ask_id) {
            (Some(op), Some(ask)) => format!("[{label}: operation {op}, ask {ask}]"),
            (Some(op), None) => format!("[{label}: operation {op}]"),
            (None, Some(ask)) => format!("[{label}: ask {ask}]"),
            (None, None) => format!("[{label}]"),
        };
        let mut facts = Vec::new();
        match self.status {
            ShellStatus::Done => {
                if let Some(op) = &self.operation_id {
                    facts.push(format!("[operation {op}]"));
                }
            }
            ShellStatus::Error => facts.push(match self.exit_code {
                Some(code) => format!("[exit {code}]"),
                None => "[failed; no exit code]".to_string(),
            }),
            ShellStatus::Rejected => facts.push("[rejected: the program did not run]".to_string()),
            ShellStatus::Running => facts.push(operation("running in the background")),
            ShellStatus::Waiting => facts.push(operation("waiting for approval; not run yet")),
            ShellStatus::Timeout => facts.push(operation("timed out waiting; the command may still be running")),
            ShellStatus::StreamClosed => facts.push(operation("the outcome never arrived")),
        }
        if let Some(data) = &self.data {
            facts.push(format!("[data] {data}"));
        }
        if let Some(latch) = &self.latch {
            facts.push(format!("[latch] {latch}"));
        }
        if let Some(cwd) = &self.cwd {
            facts.push(format!("[cwd now {cwd}]"));
        }
        if let Some(warning) = &self.warning {
            facts.push(format!("[warning: {warning}]"));
        }
        let mut text = if clean.is_empty() && facts.is_empty() { "[no output]".to_string() } else { clean.to_string() };
        for fact in facts {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&fact);
        }
        text
    }

    /// The keys this envelope always serializes, in declaration order. The
    /// declared output schema is checked against this list, so a new field
    /// cannot reach the wire undocumented.
    pub const KEYS: &'static [&'static str] = &[
        "stdout",
        "stderr",
        "exit_code",
        "status",
        "did_spill",
        "data",
        "latch",
        "block_id",
        "operation_id",
        "ask_id",
        "content_type",
        "ephemeral",
        "elapsed_ms",
        "error",
        "cwd",
        "warning",
    ];

    /// The declared result shape (an MCP `Tool.outputSchema`).
    ///
    /// Hand-written rather than derived, so `kaijutsu-types` does not take a
    /// JSON-schema dependency for one function. `schema_declares_every_key`
    /// below fails when a field is added here without a matching property, so
    /// the two cannot drift apart silently.
    pub fn output_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "stdout": { "type": "string" },
                "stderr": { "type": "string" },
                "exit_code": {
                    "type": ["integer", "null"],
                    "description": "null = no command exit (refused, hook replacement, or unavailable); never evidence of success"
                },
                "status": {
                    "type": "string",
                    "enum": ["done", "error", "rejected", "running", "waiting", "timeout", "stream_closed"]
                },
                "did_spill": {
                    "type": ["boolean", "null"],
                    "description": "output was cut to a head and tail whose last line says where the rest is; null when this path cannot tell"
                },
                "data": { "description": "kj structured payload when present, else null" },
                "latch": { "description": "kj confirmation-gate re-run hint when present, else null" },
                "block_id": { "type": ["string", "null"] },
                "ask_id": { "type": ["string", "null"] },
                "operation_id": {
                    "type": ["string", "null"],
                    "description": "durable shell operation handle, retained while waiting and after completion"
                },
                "content_type": { "type": ["string", "null"] },
                "ephemeral": { "type": ["boolean", "null"] },
                "elapsed_ms": { "type": ["integer", "null"] },
                "error": {
                    "type": ["string", "null"],
                    "description": "why the outcome never arrived, or why the program was refused"
                },
                "cwd": {
                    "type": ["string", "null"],
                    "description": "the shell's working directory after this call, when the call changed it; null when it did not, or when this path cannot tell"
                },
                "warning": {
                    "type": ["string", "null"],
                    "description": "a condition to correct before going on, such as a shell left at /; null when there is none"
                }
            },
            "required": [
                "stdout", "stderr", "exit_code", "status", "did_spill", "data", "latch",
                "block_id", "operation_id", "ask_id", "content_type", "ephemeral", "elapsed_ms", "error", "cwd", "warning"
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key must survive serialization even when its value is unset.
    /// A caller scripting against this envelope reads `.exit_code` without
    /// first testing whether the key exists; `skip_serializing_if` anywhere
    /// in this struct would put the shape flip back.
    #[test]
    fn a_clean_success_reads_as_its_output_alone() {
        let mut env = ShellEnvelope::new(ShellStatus::Done);
        env.exit_code = Some(0);
        env.elapsed_ms = Some(12);
        env.block_id = Some("b".into());
        env.content_type = Some("text/plain".into());
        assert_eq!(env.model_text("hello\n"), "hello\n");
        assert_eq!(env.model_text(""), "[no output]", "an empty result still has one shape");
    }

    #[test]
    fn a_failure_names_its_exit_after_the_output() {
        let mut env = ShellEnvelope::new(ShellStatus::Error);
        env.exit_code = Some(2);
        assert_eq!(env.model_text("no such file"), "no such file\n[exit 2]");
        assert_eq!(env.model_text(""), "[exit 2]");
        env.exit_code = None;
        assert_eq!(env.model_text("boom\n"), "boom\n[failed; no exit code]");
    }

    #[test]
    fn handles_and_structured_facts_ride_below_the_output() {
        let mut env = ShellEnvelope::new(ShellStatus::Running);
        env.operation_id = Some("op-1".into());
        assert_eq!(env.model_text(""), "[running in the background: operation op-1]");
        let mut env = ShellEnvelope::new(ShellStatus::Waiting);
        env.operation_id = Some("op-2".into());
        env.ask_id = Some("ask-9".into());
        assert_eq!(env.model_text(""), "[waiting for approval; not run yet: operation op-2, ask ask-9]");
        let mut env = ShellEnvelope::new(ShellStatus::Done);
        env.exit_code = Some(0);
        env.did_spill = Some(true);
        env.data = Some(serde_json::json!(["a", "b"]));
        env.latch = Some(serde_json::json!({"command": "kj context archive x --confirm"}));
        assert_eq!(env.model_text("listed\n"),
            "listed\n[data] [\"a\",\"b\"]\n[latch] {\"command\":\"kj context archive x --confirm\"}");
        assert_eq!(ShellEnvelope::new(ShellStatus::Rejected).model_text("parse error at 1:4"),
            "parse error at 1:4\n[rejected: the program did not run]");
    }

    /// A call that changed the shell's directory says where it now is, on
    /// its own line after the output; a call that did not says nothing.
    #[test]
    fn a_changed_cwd_reads_as_one_line() {
        let mut env = ShellEnvelope::new(ShellStatus::Done);
        env.exit_code = Some(0);
        env.cwd = Some("/work/sub".into());
        assert_eq!(env.model_text(""), "[cwd now /work/sub]", "a silent `cd` still says where it went");
        assert_eq!(env.model_text("built\n"), "built\n[cwd now /work/sub]");
        env.status = ShellStatus::Error;
        env.exit_code = Some(1);
        assert_eq!(env.model_text("no such file\n"), "no such file\n[exit 1]\n[cwd now /work/sub]");
    }

    /// A call that leaves the shell at `/` says so on every call, after the
    /// output and the other facts, whether or not the call changed the cwd.
    #[test]
    fn a_warning_reads_as_one_line() {
        let mut env = ShellEnvelope::new(ShellStatus::Done);
        env.exit_code = Some(0);
        env.warning = root_cwd_warning(std::path::Path::new("/"));
        assert_eq!(env.model_text(""),
            "[warning: cwd is /, which is almost always a mistake; cd to your work tree]");
        env.cwd = Some("/".into());
        assert_eq!(env.model_text("bin\netc\n"),
            "bin\netc\n[cwd now /]\n[warning: cwd is /, which is almost always a mistake; cd to your work tree]");
    }

    /// Only the filesystem root draws the warning.
    #[test]
    fn only_the_root_is_a_root_cwd() {
        for root in ["/", "//"] {
            assert!(is_root_cwd(std::path::Path::new(root)), "{root}");
        }
        for not_root in ["/home", "/tmp/", "work", ""] {
            assert!(!is_root_cwd(std::path::Path::new(not_root)), "{not_root}");
            assert_eq!(root_cwd_warning(std::path::Path::new(not_root)), None, "{not_root}");
        }
    }

    /// An envelope stored without `cwd` still reads, as unchanged: the
    /// shell operation table keeps completed envelopes.
    #[test]
    fn a_stored_envelope_without_cwd_reads_as_unchanged() {
        let mut stored = ShellEnvelope::new(ShellStatus::Done).to_value();
        stored.as_object_mut().unwrap().remove("cwd");
        let read: ShellEnvelope = serde_json::from_value(stored).expect("a stored envelope without cwd");
        assert_eq!(read.cwd, None);
    }

    #[test]
    fn every_key_is_present_even_when_unset() {
        let v = ShellEnvelope::new(ShellStatus::Done).to_value();
        let obj = v.as_object().expect("envelope serializes to an object");
        for key in ShellEnvelope::KEYS {
            assert!(obj.contains_key(*key), "key {key} missing from {v}");
        }
        assert_eq!(
            obj.len(),
            ShellEnvelope::KEYS.len(),
            "KEYS is out of step with the struct: {v}"
        );
    }

    /// The declared schema and the struct are two statements of one shape.
    /// This is what catches a field added to the struct but not the schema.
    #[test]
    fn schema_declares_every_key() {
        let schema = ShellEnvelope::output_schema();
        let props = schema["properties"]
            .as_object()
            .expect("schema has properties");
        for key in ShellEnvelope::KEYS {
            assert!(props.contains_key(*key), "schema is missing property {key}");
        }
        assert_eq!(
            props.len(),
            ShellEnvelope::KEYS.len(),
            "schema declares a property the struct does not serialize"
        );
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema has required")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        assert_eq!(
            required,
            ShellEnvelope::KEYS,
            "every key is always present, so every key is required"
        );
    }

    /// Amy's rule, 2026-09-04: a nonzero exit is an error on both builders.
    #[test]
    fn nonzero_exit_is_an_error_and_zero_is_not() {
        assert_eq!(ShellEnvelope::status_for_exit(0), ShellStatus::Done);
        assert_eq!(ShellEnvelope::status_for_exit(1), ShellStatus::Error);
        assert_eq!(ShellEnvelope::status_for_exit(-1), ShellStatus::Error);

        let mut env = ShellEnvelope::new(ShellEnvelope::status_for_exit(1));
        env.exit_code = Some(1);
        assert!(env.is_error());

        let mut ok = ShellEnvelope::new(ShellEnvelope::status_for_exit(0));
        ok.exit_code = Some(0);
        assert!(!ok.is_error());
    }

    /// A started background command is not a failure, and neither is a
    /// silent success. The statuses that DO flag are the ones a caller must
    /// not mistake for output.
    #[test]
    fn only_failing_statuses_flag() {
        assert!(!ShellStatus::Done.is_error());
        assert!(!ShellStatus::Running.is_error());
        assert!(ShellStatus::Error.is_error());
        assert!(ShellStatus::Rejected.is_error());
        assert!(ShellStatus::Timeout.is_error());
        assert!(ShellStatus::StreamClosed.is_error());
    }

    /// The wire spelling is what callers match on; a rename here is a
    /// breaking change to every script reading `.status`.
    #[test]
    fn status_serializes_as_its_public_spelling() {
        for (status, spelling) in [
            (ShellStatus::Done, "done"),
            (ShellStatus::Error, "error"),
            (ShellStatus::Rejected, "rejected"),
            (ShellStatus::Running, "running"),
            (ShellStatus::Timeout, "timeout"),
            (ShellStatus::StreamClosed, "stream_closed"),
        ] {
            assert_eq!(status.as_str(), spelling);
            let env = ShellEnvelope::new(status).to_value();
            assert_eq!(env["status"], serde_json::json!(spelling));
        }
    }

    /// Only a real envelope round-trips. A body that is not one must not be
    /// mistaken for one — the agentic loop uses this to decide whether the
    /// durable block gets the command's output or the tool's text verbatim.
    #[test]
    fn only_a_real_envelope_is_recovered_from_a_body() {
        let mut env = ShellEnvelope::new(ShellStatus::Done);
        env.stdout = "hi\n".into();
        env.exit_code = Some(0);
        let body = env.to_value().to_string();
        let back = ShellEnvelope::from_tool_result(&body).expect("an envelope round-trips");
        assert_eq!(back.stdout, "hi\n");
        assert_eq!(back.exit_code, Some(0));

        for not_an_envelope in [
            "hello world",
            "",
            "   ",
            "[exit 1]",
            r#"{"some":"other tool"}"#,
            r#"{"stdout":"partial","exit_code":0}"#,
            "{not json at all",
        ] {
            assert!(
                ShellEnvelope::from_tool_result(not_an_envelope).is_none(),
                "must not read {not_an_envelope:?} as an envelope"
            );
        }
    }

    /// The durable block holds what a person reads, not the envelope. Both
    /// streams appear, and a refusal shows the reason it rode in `error`.
    #[test]
    fn readable_output_is_the_command_output_not_the_envelope() {
        let mut env = ShellEnvelope::new(ShellStatus::Error);
        env.stdout = "some output".into();
        env.stderr = "a warning".into();
        env.exit_code = Some(1);
        let text = env.readable_output();
        assert_eq!(text, "some output\n[stderr]\na warning", "a `[stderr]` line marks where stderr starts");
        assert!(!text.contains("exit_code"), "no envelope keys in a block: {text}");

        env.stdout = "ends in a newline\n".into();
        assert_eq!(env.readable_output(), "ends in a newline\n[stderr]\na warning");

        env.stdout.clear();
        assert_eq!(env.readable_output(), "[stderr]\na warning", "stderr alone is still marked");

        let mut refused = ShellEnvelope::new(ShellStatus::Rejected);
        refused.error = Some("parse error at 1:6".into());
        assert_eq!(refused.readable_output(), "[stderr]\nparse error at 1:6", "a refusal reason reads as stderr");

        let mut both = ShellEnvelope::new(ShellStatus::Rejected);
        both.stderr = "first".into();
        both.error = Some("second".into());
        assert_eq!(both.readable_output(), "[stderr]\nfirst\nsecond", "one marker for stderr and the reason");

        let mut out_only = ShellEnvelope::new(ShellStatus::Done);
        out_only.stdout = "just stdout\n".into();
        assert_eq!(out_only.readable_output(), "just stdout\n", "no marker without stderr");

        let quiet = ShellEnvelope::new(ShellStatus::Done);
        assert_eq!(quiet.readable_output(), "", "a silent command reads as nothing");
    }

    /// After ANSI stripping, the model's envelope and the durable block must
    /// carry one text — two copies with different byte offsets is what every
    /// edit and exclusion range would then disagree about.
    #[test]
    fn the_clean_output_replaces_both_streams_and_keeps_the_verdict() {
        let mut env = ShellEnvelope::new(ShellStatus::Error);
        env.stdout = "\u{1b}[32mgreen\u{1b}[0m".into();
        env.stderr = "warned".into();
        env.exit_code = Some(2);
        env.did_spill = Some(true);

        let cleaned = env.clone().with_clean_output("green\nwarned");
        assert_eq!(cleaned.stdout, "green\nwarned");
        assert_eq!(cleaned.stderr, "", "stderr is now inside stdout, not repeated");
        assert_eq!(cleaned.error, None);
        assert_eq!(
            cleaned.exit_code,
            Some(2),
            "the verdict survives — a caller was told to read exit_code"
        );
        assert_eq!(cleaned.status, ShellStatus::Error);
        assert_eq!(cleaned.did_spill, Some(true));
        assert!(cleaned.is_error());

        // Every key is still there after the rewrite.
        let obj = cleaned.to_value();
        let obj = obj.as_object().expect("an object");
        for key in ShellEnvelope::KEYS {
            assert!(obj.contains_key(*key), "key {key} lost in the rewrite");
        }
    }

    /// An empty successful command and a rejected one must not read alike.
    /// `cat /dev/null` and a program kaish refused both have empty stdout;
    /// the worknote that started this work reported exactly that pair as
    /// indistinguishable.
    #[test]
    fn empty_success_and_rejection_are_distinguishable() {
        let mut empty = ShellEnvelope::new(ShellStatus::Done);
        empty.exit_code = Some(0);

        let mut refused = ShellEnvelope::new(ShellStatus::Rejected);
        refused.error = Some("parse error at 1:6".into());

        assert_eq!(empty.stdout, refused.stdout, "both have empty stdout");
        assert_ne!(empty.status, refused.status);
        assert!(!empty.is_error() && refused.is_error());
        assert!(empty.error.is_none() && refused.error.is_some());
    }
}
