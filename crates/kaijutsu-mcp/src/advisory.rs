//! Advisory risk scoring for a Claude Code `Bash` call, off the reply path.
//!
//! **The reply is never delayed and never changed.** The listener answers
//! `tool.before` first and hands the command to [`AdvisoryConfig::spawn`]
//! second; everything in this module runs in a detached task whose only
//! product is a JSONL row. A scorer that is down, slow, or answering
//! nonsense costs the caller nothing — that is the contract
//! `advisory_scoring_never_delays_the_reply` pins, and any change here that
//! could make the hook wait is a defect, not a tuning decision.
//!
//! The clause cut comes from `kaijutsu_kernel::kj::plan_clauses`, over a
//! plan built in this process with the linked `kaish-kernel`. Planning in
//! process is the point: an out-of-process `kaish` binary on `PATH` can
//! change version underneath a running session, and the rows then mix two
//! clause populations with nothing on them to say so.
//!
//! Rows land in `~/.cache/claude-hooks/kaijutsu-advisory.jsonl`, a different
//! file from the Python hook's `lfm2d-advisory.jsonl`, so both can run over
//! the same session and be compared row for row. The shape mirrors that
//! file's: `ts`, `cwd`, `command`, `session_id`, `regex`, `disagree`, and an
//! `lfm2d` object. `regex` and `disagree` are always null here — this
//! process runs no regex guard and has nothing to disagree with. `source`
//! says which producer wrote the row.
//!
//! The file holds command text, so it is created `0600`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

/// Per-request timeout for one `/v1/cascade` call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Most clauses one cascade call may carry. Matches the Python hook's
/// `LFM2D_CASCADE_MAX_CLAUSES` default, so rows from the two producers are
/// comparable; a command planning more sends the first 20 and records
/// `clauses_truncated`.
const MAX_CLAUSES: usize = 20;

/// How long the breaker stays open after a failed POST.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(60);

/// Which producer wrote the row.
const ROW_SOURCE: &str = "kaijutsu-mcp";

/// Where a scored row goes and who to ask.
///
/// One per listener, not a global: the breaker lives here, so a test gets
/// its own and cannot be steered by another test's failed call.
#[derive(Debug)]
pub struct AdvisoryConfig {
    /// Scorer base URL — `/v1/cascade` is appended.
    base_url: String,
    /// The JSONL file rows are appended to.
    log_path: PathBuf,
    breaker: Mutex<Breaker>,
}

impl AdvisoryConfig {
    /// Read the scorer from the process environment.
    ///
    /// `None` when `LFM2D_URL` is unset, which is the ordinary case for a
    /// session that has not opted in. Called once per process, so the
    /// info-level note it logs is not per call.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("LFM2D_URL").ok().filter(|u| !u.is_empty());
        let Some(base_url) = base_url else {
            tracing::info!(
                "LFM2D_URL unset — Bash advisory scoring is off for this process"
            );
            return None;
        };
        let log_path = default_log_path()?;
        tracing::info!(
            url = %base_url,
            log = %log_path.display(),
            "Bash advisory scoring enabled"
        );
        Some(Self::new(base_url, log_path))
    }

    /// Build a config directly, for a caller that supplies both endpoints.
    pub fn new(base_url: impl Into<String>, log_path: impl Into<PathBuf>) -> Self {
        Self {
            base_url: base_url.into(),
            log_path: log_path.into(),
            breaker: Mutex::new(Breaker::default()),
        }
    }

    /// Score `call` in a detached task and return immediately.
    ///
    /// Nothing this spawns can reach the caller: the task owns its inputs,
    /// returns nothing, and reports every failure to `tracing` at debug.
    pub fn spawn(self: &std::sync::Arc<Self>, call: AdvisoryCall) {
        let config = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            config.score(call).await;
        });
    }

    async fn score(&self, call: AdvisoryCall) {
        // The guard is dropped before the first `await`: a `std::sync`
        // guard held across one makes the whole task non-`Send`, and the
        // task has to be spawnable for the reply to stay unblocked.
        let check = match self.breaker.lock() {
            Ok(mut b) => Ok(b.check()),
            Err(e) => Err(e.to_string()),
        };
        match check {
            Ok(BreakerCheck::OpenReport) => {
                let lfm2d = json!({
                    "ok": false,
                    "endpoint": "cascade",
                    "error": "circuit_open",
                    "detail": format!(
                        "a POST failed; not retrying for {}s",
                        BREAKER_COOLDOWN.as_secs()
                    ),
                });
                self.append_row(build_row(&call, lfm2d)).await;
                return;
            }
            Ok(BreakerCheck::OpenSilent) => return,
            Ok(BreakerCheck::Closed) => {}
            Err(e) => {
                tracing::debug!("advisory breaker lock poisoned: {e}");
                return;
            }
        }

        let planned = plan_clauses(&call.command);
        if planned.clauses.is_empty() {
            return;
        }

        let started = Instant::now();
        let outcome = self.post_cascade(&planned.clauses).await;
        let latency_ms = round_tenths(started.elapsed().as_secs_f64() * 1000.0);

        let ok = outcome.is_ok();
        if let Ok(mut breaker) = self.breaker.lock() {
            breaker.record(ok);
        }

        let mut lfm2d = match outcome {
            Ok(response) => cascade_verdict(&response, latency_ms),
            Err(e) => json!({
                "ok": false,
                "endpoint": "cascade",
                "error": e.kind,
                "detail": e.detail,
                "latency_ms": latency_ms,
            }),
        };
        if let Some(obj) = lfm2d.as_object_mut() {
            obj.insert("split_path".into(), json!(planned.split_path));
            obj.insert("plan".into(), planned.meta);
        }
        self.append_row(build_row(&call, lfm2d)).await;
    }

    async fn post_cascade(&self, clauses: &[String]) -> Result<CascadeResponse, PostError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| PostError::new("client_build", e))?;
        let url = format!("{}/v1/cascade", self.base_url.trim_end_matches('/'));
        let response = client
            .post(&url)
            .json(&json!({ "clauses": clauses }))
            .send()
            .await
            .map_err(|e| PostError::new("request", e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(PostError {
                kind: format!("http {}", status.as_u16()),
                detail: truncate(&response.text().await.unwrap_or_default(), 200),
            });
        }
        response
            .json::<CascadeResponse>()
            .await
            .map_err(|e| PostError::new("decode", e))
    }

    /// Append one row. Best effort: a log that cannot be written must not
    /// escalate into anything the session notices beyond a debug line.
    async fn append_row(&self, row: Value) {
        if let Err(e) = write_row(&self.log_path, &row).await {
            tracing::debug!(path = %self.log_path.display(), "advisory row not written: {e}");
        }
    }
}

/// One Bash call to score, owned by the spawned task.
#[derive(Debug, Clone)]
pub struct AdvisoryCall {
    /// The command as Claude Code presented it.
    pub command: String,
    /// The caller's working directory, when the hook event carried one.
    pub cwd: Option<String>,
    /// The Claude Code session id the listener holds, when it has seen one.
    pub session_id: Option<String>,
}

// ── the clause list ────────────────────────────────────────────────────

/// What planning produced: the clauses to send, how they were cut, and the
/// `plan` object that goes on the row.
struct PlannedClauses {
    clauses: Vec<String>,
    split_path: &'static str,
    meta: Value,
}

/// Plan `command` in process and render its clauses.
///
/// On a parse failure the whole command becomes one clause and the row says
/// so twice — `split_path: "whole"` and `plan.plan_error`. There is no
/// second splitter: a fallback that cuts shell text by hand fabricates
/// clauses the shell never had, and the Python hook's own splitter exists
/// only because it could not link kaish.
fn plan_clauses(command: &str) -> PlannedClauses {
    let mut meta = json!({
        "kaish_version": kaish_kernel::KAISH_VERSION,
        "kaish_git_hash": kaish_kernel::KAISH_GIT_HASH,
        "kaish_build_date": kaish_kernel::KAISH_BUILD_DATE,
    });

    let (mut clauses, split_path, statement_count, plan_error) =
        match kaish_kernel::plan_program(command) {
            Ok(statements) => {
                let clauses = kaijutsu_kernel::kj::plan_clauses::render_clauses(&statements)
                    .into_iter()
                    .map(|c| c.clause)
                    .collect::<Vec<_>>();
                (clauses, "kaish_plan", statements.len(), None)
            }
            Err(errors) => {
                let detail = errors
                    .iter()
                    .map(|e| e.format(command))
                    .collect::<Vec<_>>()
                    .join("\n");
                (
                    vec![command.to_string()],
                    "whole",
                    0,
                    Some(truncate(&detail, 400)),
                )
            }
        };

    let planned = clauses.len();
    let truncated = planned.saturating_sub(MAX_CLAUSES);
    clauses.truncate(MAX_CLAUSES);

    if let Some(obj) = meta.as_object_mut() {
        obj.insert("statement_count".into(), json!(statement_count));
        obj.insert("clauses_planned".into(), json!(planned));
        obj.insert("clauses_truncated".into(), json!(truncated));
        if let Some(err) = plan_error {
            obj.insert("plan_error".into(), json!(err));
        }
    }

    PlannedClauses {
        clauses,
        split_path,
        meta,
    }
}

// ── the scorer's answer ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CascadeResponse {
    winner: CascadeWinner,
    #[serde(default)]
    clauses: Vec<CascadeClause>,
    #[serde(default)]
    models: Vec<CascadeModel>,
}

#[derive(Debug, Deserialize)]
struct CascadeWinner {
    index: usize,
    #[serde(default)]
    clause: String,
    #[serde(default)]
    severity_scores: Value,
}

#[derive(Debug, Deserialize)]
struct CascadeClause {
    #[serde(default)]
    clause: String,
    #[serde(default)]
    top_severity: Option<String>,
    #[serde(default)]
    severity_scores: Value,
}

#[derive(Debug, Deserialize)]
struct CascadeModel {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    weight_hash: Option<String>,
}

/// Turn one cascade response into the row's `lfm2d` object.
///
/// `top` reads the winning clause's own `top_severity` rather than deriving
/// an argmax here — the label vocabulary belongs to the deployed checkpoint
/// and has changed wholesale before, so nothing in this file compares a
/// label against a constant.
///
/// A winner index that names no clause is a malformed response, not a
/// missing field to paper over: the row records `ok: false` and says which
/// index was out of range.
fn cascade_verdict(response: &CascadeResponse, latency_ms: f64) -> Value {
    let Some(winning) = response.clauses.get(response.winner.index) else {
        return json!({
            "ok": false,
            "endpoint": "cascade",
            "error": "malformed",
            "detail": format!(
                "winner index {} names no clause among {}",
                response.winner.index,
                response.clauses.len()
            ),
            "latency_ms": latency_ms,
        });
    };
    let classifier = response.models.first();
    json!({
        "ok": true,
        "endpoint": "cascade",
        "top": winning.top_severity,
        "scores": response.winner.severity_scores,
        "winner_index": response.winner.index,
        "winner_clause": response.winner.clause,
        "clause_count": response.clauses.len(),
        "clauses": response.clauses.iter().map(|c| json!({
            "clause": c.clause,
            "top": c.top_severity,
            "scores": c.severity_scores,
        })).collect::<Vec<_>>(),
        "model_id": classifier.and_then(|m| m.model_id.clone()),
        "weight_hash": classifier.and_then(|m| m.weight_hash.clone()),
        "latency_ms": latency_ms,
    })
}

/// A POST that did not produce a verdict.
struct PostError {
    kind: String,
    detail: String,
}

impl PostError {
    fn new(kind: &str, e: impl std::fmt::Display) -> Self {
        Self {
            kind: kind.to_string(),
            detail: truncate(&e.to_string(), 200),
        }
    }
}

// ── the row ────────────────────────────────────────────────────────────

/// Assemble one advisory row around an already-built `lfm2d` verdict.
fn build_row(call: &AdvisoryCall, lfm2d: Value) -> Value {
    json!({
        "ts": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        "cwd": call.cwd,
        "command": call.command,
        "source": ROW_SOURCE,
        "session_id": call.session_id,
        // This process runs no regex guard, so there is no second verdict
        // and nothing to bucket. Present and null rather than absent: a
        // reader that joins the two producers' files sees the same keys.
        "regex": Value::Null,
        "disagree": Value::Null,
        "lfm2d": lfm2d,
    })
}

/// Append one row as a JSON line, creating the file `0600` if absent.
async fn write_row(path: &Path, row: &Value) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .await?;
    let mut line = serde_json::to_string(row)?;
    line.push('\n');
    file.write_all(line.as_bytes()).await
}

fn default_log_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("claude-hooks").join("kaijutsu-advisory.jsonl"))
}

fn round_tenths(ms: f64) -> f64 {
    (ms * 10.0).round() / 10.0
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

// ── the breaker ────────────────────────────────────────────────────────

/// Skip scoring for [`BREAKER_COOLDOWN`] after a failed POST.
///
/// Without one, an outage costs every Bash call in the session a full
/// [`REQUEST_TIMEOUT`] of daemon work for as long as it lasts. The verdict
/// is advisory, so re-learning "still down" once a minute is enough.
///
/// In memory only, and deliberately not silent: the first skipped call of
/// each open period writes a `circuit_open` row, so a quiet stretch in the
/// log is distinguishable from a stretch where nothing was asked.
#[derive(Debug, Default)]
struct Breaker {
    open_until: Option<Instant>,
    reported: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum BreakerCheck {
    Closed,
    /// Open, and this is the first skipped call — write the row.
    OpenReport,
    /// Open, and the row for this period is already written.
    OpenSilent,
}

impl Breaker {
    fn check(&mut self) -> BreakerCheck {
        match self.open_until {
            Some(until) if Instant::now() < until => {
                if self.reported {
                    BreakerCheck::OpenSilent
                } else {
                    self.reported = true;
                    BreakerCheck::OpenReport
                }
            }
            _ => {
                self.open_until = None;
                self.reported = false;
                BreakerCheck::Closed
            }
        }
    }

    fn record(&mut self, ok: bool) {
        if ok {
            self.open_until = None;
            self.reported = false;
        } else {
            self.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
            self.reported = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The live scorer's answer for `{"clauses":["ls -la"]}`, trimmed to the
    /// fields this module reads plus one it ignores.
    const CANNED_CASCADE: &str = r#"{
        "winner": {
            "index": 1,
            "clause": "rm -rf /tmp/x",
            "severity_scores": {"data-critical": 0.81, "informative": 0.12, "situation-normal": 0.07}
        },
        "lane": {"route": "shell", "cosine": 0.99},
        "clauses": [
            {"index": 0, "clause": "ls -la", "top_severity": "informative",
             "severity_scores": {"data-critical": 0.01, "informative": 0.98, "situation-normal": 0.01}},
            {"index": 1, "clause": "rm -rf /tmp/x", "top_severity": "data-critical",
             "severity_scores": {"data-critical": 0.81, "informative": 0.12, "situation-normal": 0.07}}
        ],
        "models": [
            {"model_id": "kube_ordinal_v10", "weight_hash": "e90e0ba8"},
            {"model_id": "LFM2.5-Encoder-350M-Prompt-Router", "weight_hash": "9fab23ee"}
        ]
    }"#;

    fn canned() -> CascadeResponse {
        serde_json::from_str(CANNED_CASCADE).expect("canned cascade response must parse")
    }

    fn call(command: &str) -> AdvisoryCall {
        AdvisoryCall {
            command: command.to_string(),
            cwd: Some("/home/atobey/src/kaijutsu".to_string()),
            session_id: Some("a1b2c3d4-e5f6".to_string()),
        }
    }

    fn temp_log(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!("kaijutsu-advisory-{tag}-{}-{nanos}", std::process::id()))
            .join("kaijutsu-advisory.jsonl")
    }

    /// The winner's verdict is what the row reports at the top level, and
    /// the command text round-trips verbatim.
    #[test]
    fn a_row_carries_the_winning_clause_and_the_command() {
        let lfm2d = cascade_verdict(&canned(), 123.4);
        let row = build_row(&call("ls -la && rm -rf /tmp/x"), lfm2d);

        assert_eq!(row["command"], "ls -la && rm -rf /tmp/x");
        assert_eq!(row["source"], ROW_SOURCE);
        assert_eq!(row["session_id"], "a1b2c3d4-e5f6");
        assert!(row["regex"].is_null() && row["disagree"].is_null());

        let lfm2d = &row["lfm2d"];
        assert_eq!(lfm2d["ok"], true);
        assert_eq!(lfm2d["endpoint"], "cascade");
        assert_eq!(lfm2d["top"], "data-critical");
        assert_eq!(lfm2d["winner_index"], 1);
        assert_eq!(lfm2d["winner_clause"], "rm -rf /tmp/x");
        assert_eq!(lfm2d["clause_count"], 2);
        assert_eq!(lfm2d["model_id"], "kube_ordinal_v10");
        assert_eq!(lfm2d["weight_hash"], "e90e0ba8");
        assert_eq!(lfm2d["latency_ms"], 123.4);
        assert_eq!(lfm2d["clauses"][0]["clause"], "ls -la");
        assert_eq!(lfm2d["clauses"][0]["top"], "informative");
    }

    /// A winner index naming no clause is recorded as malformed, not
    /// silently reported as some other clause's verdict.
    #[test]
    fn an_out_of_range_winner_is_recorded_as_malformed() {
        let mut response = canned();
        response.winner.index = 7;
        let lfm2d = cascade_verdict(&response, 1.0);
        assert_eq!(lfm2d["ok"], false);
        assert_eq!(lfm2d["error"], "malformed");
    }

    /// Clauses past the cap are dropped from the request and counted on the
    /// row — a monster command must not be silently scored in part.
    #[test]
    fn clauses_are_capped_and_the_drop_is_counted() {
        let command = (0..25)
            .map(|i| format!("echo {i}"))
            .collect::<Vec<_>>()
            .join(" && ");
        let planned = plan_clauses(&command);

        assert_eq!(planned.clauses.len(), MAX_CLAUSES);
        assert_eq!(planned.split_path, "kaish_plan");
        assert_eq!(planned.meta["clauses_planned"], 25);
        assert_eq!(planned.meta["clauses_truncated"], 5);
        assert_eq!(planned.clauses[0], "echo 0");
        assert_eq!(planned.clauses[MAX_CLAUSES - 1], "echo 19");
    }

    /// A command kaish cannot parse is scored whole, and the row says so —
    /// there is no second splitter to fabricate clauses.
    #[test]
    fn a_parse_failure_scores_the_whole_command() {
        let command = "echo 'unterminated";
        let planned = plan_clauses(command);

        assert_eq!(planned.clauses, vec![command.to_string()]);
        assert_eq!(planned.split_path, "whole");
        assert_eq!(planned.meta["statement_count"], 0);
        assert_eq!(planned.meta["clauses_planned"], 1);
        assert!(
            planned.meta["plan_error"].is_string(),
            "the parse diagnostics must reach the row, got {:?}",
            planned.meta["plan_error"]
        );
    }

    /// The version the rows are windowed by is the linked crate's, read
    /// from `kaish-kernel` itself — never a string typed here, and never a
    /// binary's `--version` that can change under a running session.
    #[test]
    fn the_row_names_the_linked_kaish() {
        let planned = plan_clauses("ls");
        assert_eq!(planned.meta["kaish_version"], kaish_kernel::KAISH_VERSION);
        assert!(
            planned.meta["kaish_version"]
                .as_str()
                .is_some_and(|v| v.starts_with(char::is_numeric)),
            "the bare semver is what sorts; got {:?}",
            planned.meta["kaish_version"]
        );
    }

    /// End to end against a local socket answering the canned response: the
    /// clauses are cut per command, the POST lands, and one row is
    /// appended.
    #[tokio::test]
    async fn a_scored_call_appends_one_row() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            serve_one_cascade(listener).await;
        });

        let log = temp_log("scored");
        let config = Arc::new(AdvisoryConfig::new(format!("http://{addr}"), &log));
        config.score(call("ls -la && rm -rf /tmp/x")).await;

        let written = tokio::fs::read_to_string(&log).await.expect("row written");
        let row: Value = serde_json::from_str(written.trim()).expect("row is one JSON line");
        assert_eq!(row["lfm2d"]["ok"], true);
        assert_eq!(row["lfm2d"]["split_path"], "kaish_plan");
        assert_eq!(row["lfm2d"]["plan"]["clauses_planned"], 2);
        assert_eq!(row["lfm2d"]["winner_clause"], "rm -rf /tmp/x");
    }

    /// The file is created `0600`: it holds command text, which can carry a
    /// secret the author never meant to publish.
    #[tokio::test]
    async fn the_log_is_created_private() {
        use std::os::unix::fs::PermissionsExt;

        let log = temp_log("perms");
        write_row(&log, &json!({"ts": 1.0})).await.unwrap();

        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "advisory rows must not be world-readable");
    }

    /// An unreachable scorer writes one failure row, then one `circuit_open`
    /// row, then nothing — the breaker must not re-report every call.
    #[tokio::test]
    async fn the_breaker_reports_once_per_open_period() {
        // Port 1 on loopback refuses immediately; nothing is ever listening.
        let log = temp_log("breaker");
        let config = Arc::new(AdvisoryConfig::new("http://127.0.0.1:1", &log));

        config.score(call("ls")).await;
        config.score(call("ls")).await;
        config.score(call("ls")).await;

        let written = tokio::fs::read_to_string(&log).await.expect("rows written");
        let rows: Vec<Value> = written
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is JSON"))
            .collect();
        assert_eq!(rows.len(), 2, "one failure row and one circuit_open row");
        assert_eq!(rows[0]["lfm2d"]["ok"], false);
        assert_eq!(rows[0]["lfm2d"]["error"], "request");
        assert_eq!(rows[1]["lfm2d"]["error"], "circuit_open");
    }

    #[test]
    fn a_success_closes_the_breaker() {
        let mut breaker = Breaker::default();
        breaker.record(false);
        assert_eq!(breaker.check(), BreakerCheck::OpenReport);
        assert_eq!(breaker.check(), BreakerCheck::OpenSilent);
        breaker.record(true);
        assert_eq!(breaker.check(), BreakerCheck::Closed);
    }

    /// Answer exactly one request with the canned cascade JSON, then stop.
    async fn serve_one_cascade(listener: tokio::net::TcpListener) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 8192];
        let _ = stream.read(&mut buf).await;
        let body = CANNED_CASCADE;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}
