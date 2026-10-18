//! A scripted council server for one scenario run (`docs/acp-fleet.md`,
//! "Council scenarios").
//!
//! It serves council v2 on 127.0.0.1 well enough for a kernel's gate: it
//! reports an identity, holds the specs it is sent, builds the contexts it
//! is sent, and answers each decision with the scenario's next verdict.
//! Every answer carries numbers `gogi_proto::math::verify` recomputes, so
//! the kernel reads it as a real council's. One verdict answers one decision, in order, and the last
//! one repeats. [`ScriptedCouncil::restart`] replaces the verdicts between
//! prompts.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use gogi_proto::context::{Context, ContextBuild, ContextBuilt, SnapshotInfo, Usage};
use gogi_proto::decision::{DecisionRequest, DecisionResponse, PoolMethod, PoolWeights};
use gogi_proto::identity::{ModelLimits, ModelStatus, ServedModel, ServerIdentity, ServerLimits};
use gogi_proto::math::{self, Row, WeightSpec};
use gogi_proto::spec::{QuestionKind, Spec, SpecRules};
use gogi_proto::{SnapshotId, sha256_hex};
use serde::Deserialize;
use serde_json::{Value, json};

/// An option of the shell spec's `verdict` question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Proceed,
    TryHarder,
    DoLess,
}

impl Verdict {
    pub fn option(self) -> &'static str {
        match self {
            Self::Proceed => "proceed",
            Self::TryHarder => "try_harder",
            Self::DoLess => "do_less",
        }
    }
}

/// An option of the shell spec's `undo` question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Undo {
    Reversible,
    Normal,
    Irreversible,
}

impl Undo {
    pub fn option(self) -> &'static str {
        match self {
            Self::Reversible => "reversible",
            Self::Normal => "normal",
            Self::Irreversible => "irreversible",
        }
    }
}

/// What the scripted council answers.
#[derive(Debug, Clone)]
pub struct Script {
    /// One per decision, in order; the last one repeats. Never empty.
    pub verdicts: Vec<Verdict>,
    /// The `undo` read every decision carries, when set.
    pub undo: Option<Undo>,
}

impl Script {
    /// The verdict for the decision numbered `n`, counted from 0.
    pub fn verdict(&self, n: usize) -> Verdict {
        self.verdicts[n.min(self.verdicts.len() - 1)]
    }
}

/// The log probability every read puts on the scripted option. The other
/// options get -8, -9, and so on, so the read is confident and its mass
/// clears any mass floor a fleet gate sets.
const CHOSEN: f64 = -0.001;

/// The template the scripted council reports in its identity and decisions.
const TEMPLATE: &str = "mk-letters-1:0123456789abcdef";

fn identity() -> Value {
    json!({"model": "fleet-council", "weight_hash": "fleet", "tokenizer_hash": "fleet", "template": TEMPLATE,
           "engine": "acp-fleet"})
}

/// The identity `GET /council/v2/identity` answers: one served model,
/// `fleet-council`. It lists no capabilities, so a kernel sends it no pins.
fn server_identity() -> ServerIdentity {
    ServerIdentity {
        models: vec![ServedModel {
            model: "fleet-council".into(),
            aliases: Vec::new(),
            status: ModelStatus::Ready,
            weight_hash: "fleet".into(),
            tokenizer_hash: "fleet".into(),
            template: TEMPLATE.into(),
            engine: "acp-fleet".into(),
            device: None,
            text_stop: None,
            limits: ModelLimits {
                context_tokens: 100000,
                decision_context_tokens: 100000,
                snapshot_bytes: 1,
                contexts_per_decision: gogi_proto::decision::MAX_CONTEXTS as u64,
                default_timeout_ms: 1000,
                text_tokens: None,
            },
            capabilities: Vec::new(),
        }],
        limits: ServerLimits { case_bytes: 100000, choice_options: 8, questions_per_spec: None, pin_bytes: None },
        capabilities: Vec::new(),
        extensions: Vec::new(),
    }
}

/// The head snapshot the scripted council reports for `context`: `snap:`
/// and the sha256 of its JSON, so the same context always has the same
/// head.
fn head_of(context: &Context) -> Result<SnapshotId> {
    let text = serde_json::to_string(context).context("a context does not encode")?;
    SnapshotId::parse(format!("snap:{}", sha256_hex(text.as_bytes()))).map_err(|e| anyhow::anyhow!(e))
}

/// The response to building `context`: its head, one token per message,
/// all of them fed, and one snapshot at its end.
fn built(context: &Context, dry_run: bool) -> Result<ContextBuilt> {
    let head = head_of(context)?;
    let tokens = context.messages.len() as u64;
    Ok(ContextBuilt {
        model: "fleet-council".into(),
        head: head.clone(),
        usage: Usage { prompt: tokens, kept: 0, fed: tokens, completion: 0, reasoning: 0 },
        from: None,
        dry_run,
        snapshots: vec![SnapshotInfo { id: head, message: tokens.saturating_sub(1), tokens, tier: None, bytes: None }],
    })
}

/// The options of `spec`'s choice question `id`, in the spec's order.
fn choice_options<'a>(spec: &'a Spec, id: &str) -> Result<Vec<&'a str>> {
    match spec.questions.iter().find(|q| q.id == id).map(|q| &q.kind) {
        Some(QuestionKind::Choice { options }) => Ok(options.iter().map(|(o, _)| o.as_str()).collect()),
        Some(_) => bail!("spec {:?} question {id:?} is not a choice question", spec.name),
        None => bail!("spec {:?} has no {id:?} question for the scripted council to answer", spec.name),
    }
}

/// An answer to `request` on `spec`: each read puts its confidence on the
/// verdict `verdict` and, when given, the `undo` option `undo`. The other
/// questions of the spec are left unanswered, so a spec that also asks the
/// rubric decides by the verdict (`docs/council.md`, "Bumper mode").
pub fn answer(request: &DecisionRequest, spec: &Spec, verdict: Verdict, undo: Option<Undo>) -> Result<Value> {
    let mut questions = vec![("verdict", verdict.option())];
    if let Some(undo) = undo {
        questions.push(("undo", undo.option()));
    }
    if let Some(ask) = &request.ask {
        for (id, _) in &questions {
            if !ask.iter().any(|q| q.as_str() == *id) {
                bail!("the decision asks only {ask:?}, not {id:?}, which the scripted council answers");
            }
        }
    }
    let contexts = &request.contexts;
    if contexts.is_empty() {
        bail!("the decision names no council context; the scripted council answers one read per context");
    }
    let pool = request.pool.clone().unwrap_or_default();
    let method = pool.method.unwrap_or(PoolMethod::Linear);
    let weights = pool.weights.unwrap_or(PoolWeights::Uniform);
    let weight_spec = match weights {
        PoolWeights::Uniform => WeightSpec::Uniform,
        PoolWeights::Mass => WeightSpec::Mass,
        PoolWeights::Given => WeightSpec::Given(pool.values.clone().context("pool weights given with no values")?),
    };
    let mut pooled_answers = serde_json::Map::new();
    let mut normalized = serde_json::Map::new();
    let mut read_answers = vec![serde_json::Map::new(); contexts.len()];
    for (id, chosen) in questions {
        let options = choice_options(spec, id)?;
        let Some(at) = options.iter().position(|o| *o == chosen) else {
            bail!("spec {:?} question {id:?} has no option {chosen:?}; it offers {options:?}", spec.name);
        };
        let mut other = 7.0;
        let logprobs: Vec<f64> = (0..options.len())
            .map(|i| {
                if i == at {
                    CHOSEN
                } else {
                    other += 1.0;
                    -other
                }
            })
            .collect();
        let row = Row::from_logprobs(&logprobs)?;
        let rows = vec![row.clone(); contexts.len()];
        let pooled = math::pool(&rows, method, &weight_spec)?;
        let by_option = |values: &[f64]| -> Value {
            options.iter().zip(values).map(|(o, v)| (o.to_string(), json!(v))).collect::<serde_json::Map<_, _>>().into()
        };
        for answers in &mut read_answers {
            answers.insert(id.to_string(), json!({
                "type": "choice",
                "choice": options[math::argmax(&row.probs)],
                "probabilities": by_option(&row.probs),
                "confidence": math::confidence(row.mass, &row.probs),
                "logprobs": by_option(&logprobs),
                "mass": row.mass,
            }));
        }
        pooled_answers.insert(id.to_string(), json!({
            "type": "choice",
            "choice": options[math::argmax(&pooled.probs)],
            "probabilities": by_option(&pooled.probs),
            "confidence": pooled.confidence(),
            "agree": pooled.agree,
            "spread": pooled.spread,
        }));
        normalized.insert(id.to_string(), json!(pooled.weights));
    }
    let reads: Vec<Value> = contexts
        .iter()
        .zip(read_answers)
        .enumerate()
        .map(|(i, (c, answers))| {
            Ok(json!({
                "context": i,
                "snapshot": head_of(c)?,
                "answers": answers,
                "rendered_sha256": "0".repeat(64),
            }))
        })
        .collect::<Result<_>>()?;
    let mut id = identity();
    id["spec_id"] = json!(request.spec_id);
    let prompt: usize = request.contexts.iter().map(|c| c.messages.len()).sum();
    let body = json!({
        "model": "fleet-council",
        "usage": {"prompt": prompt, "kept": 0, "fed": prompt, "completion": 0, "reasoning": 0},
        "answers": pooled_answers,
        "reads": reads,
        "pool": {"method": method, "weights": weights, "normalized": normalized},
        "signals": {"control_text": []},
        "identity": id,
        "queue_ms": 1.0,
        "ms": 1.0,
    });
    let decoded: DecisionResponse = serde_json::from_value(body.clone()).context("the scripted answer does not decode")?;
    math::verify(&decoded, &request.expect()).map_err(|m| anyhow::anyhow!("the scripted answer fails math::verify: {m}"))?;
    Ok(body)
}

/// The largest request body the server reads. A context upload carries a
/// conversation, so this leaves room for a long one.
pub const BODY_LIMIT: usize = 8 << 20;
/// The largest request head the server reads.
pub const HEAD_LIMIT: usize = 64 << 10;
/// The most specs the server holds.
pub const SPEC_LIMIT: usize = 64;
/// The most errors the server keeps; it counts the rest.
pub const ERROR_LIMIT: usize = 32;
/// The most bytes of one error the server keeps.
pub const ERROR_TEXT_LIMIT: usize = 1024;

/// What the server has seen, shared with its connection threads.
#[derive(Default)]
struct Shared {
    /// Specs posted to it, by spec id.
    specs: Mutex<HashMap<String, Spec>>,
    /// Each decision's answer, in order: the verdict, and the `undo` read.
    answered: Mutex<Vec<String>>,
    /// The script answering now, and the number of the decision it answers
    /// first, counted from 0 across the run.
    script: Mutex<Option<(Script, usize)>>,
    /// Requests it could not serve, each named: the first [`ERROR_LIMIT`].
    errors: Mutex<Vec<String>>,
    /// Errors past [`ERROR_LIMIT`], counted.
    errors_dropped: AtomicU64,
}

/// A council server on 127.0.0.1, alive until dropped.
pub struct ScriptedCouncil {
    base: String,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl ScriptedCouncil {
    pub fn start(script: Script) -> Result<Self> {
        if script.verdicts.is_empty() {
            bail!("a scripted council needs at least one verdict");
        }
        let listener = TcpListener::bind("127.0.0.1:0").context("bind the scripted council")?;
        let addr = listener.local_addr().context("the scripted council's address")?;
        let shared = Arc::new(Shared { script: Mutex::new(Some((script, 0))), ..Shared::default() });
        let stop = Arc::new(AtomicBool::new(false));
        let (state, stopping) = (shared.clone(), stop.clone());
        let accept = std::thread::spawn(move || {
            for sock in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(sock) = sock else { continue };
                let state = state.clone();
                std::thread::spawn(move || serve_one(sock, &state));
            }
        });
        Ok(Self { base: format!("http://{addr}"), shared, stop, accept: Some(accept) })
    }

    /// The server's address, as `[council] server` takes it.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Answer the next decision with the first of `verdicts`, then on in
    /// order; the last one repeats. The `undo` read stays as it was.
    pub fn restart(&self, verdicts: Vec<Verdict>) -> Result<()> {
        if verdicts.is_empty() {
            bail!("a scripted council needs at least one verdict");
        }
        let answered = self.shared.answered.lock().map_err(|_| anyhow::anyhow!("the answer log is poisoned"))?;
        let mut script = self.shared.script.lock().map_err(|_| anyhow::anyhow!("the script is poisoned"))?;
        let undo = script.as_ref().and_then(|(s, _)| s.undo);
        *script = Some((Script { verdicts, undo }, answered.len()));
        Ok(())
    }

    /// Each decision's answer so far, in order, such as `try_harder` or
    /// `do_less undo=irreversible`.
    pub fn answered(&self) -> Vec<String> {
        self.shared.answered.lock().map(|a| a.clone()).unwrap_or_default()
    }

    /// Every request the server could not serve, up to [`ERROR_LIMIT`],
    /// then a line counting the rest.
    pub fn errors(&self) -> Vec<String> {
        let mut errors = self.shared.errors.lock().map(|e| e.clone()).unwrap_or_default();
        let dropped = self.shared.errors_dropped.load(Ordering::SeqCst);
        if dropped > 0 {
            errors.push(format!("and {dropped} more error(s)"));
        }
        errors
    }
}

impl Drop for ScriptedCouncil {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(&self.base["http://".len()..].parse().expect("our own address"), Duration::from_secs(1));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// A request the server refuses for its size or its route, with the HTTP
/// status it answers.
#[derive(Debug)]
struct Refusal(u16, String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Refusal {}

/// One HTTP request: method, path, body. A head past [`HEAD_LIMIT`] or a
/// body past [`BODY_LIMIT`] is refused with [`Refusal`] before it is read.
fn read_request(sock: &mut TcpStream) -> Result<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        let n = sock.read(&mut chunk).context("read a request")?;
        if n == 0 {
            bail!("the connection closed before a whole request head");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
            if head.lines().any(|l| l.starts_with("transfer-encoding:")) {
                bail!("a chunked request body; the scripted council reads content-length only");
            }
            let len = match head.lines().find_map(|l| l.strip_prefix("content-length:")) {
                Some(v) => v.trim().parse::<usize>().context("content-length")?,
                None => 0,
            };
            if len > BODY_LIMIT {
                return Err(Refusal(413, format!("a request body of {len} bytes, larger than the {BODY_LIMIT} the scripted council reads")).into());
            }
            break (i + 4, len);
        }
        if buf.len() > HEAD_LIMIT {
            return Err(Refusal(431, format!("a request head larger than the {HEAD_LIMIT} bytes the scripted council reads")).into());
        }
    };
    while buf.len() < head_end + len {
        let n = sock.read(&mut chunk).context("read a request body")?;
        if n == 0 {
            bail!("the connection closed before the whole body");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut first = head.lines().next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    Ok((method, path, String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string()))
}

/// Keep `message` among the server's errors: the first [`ERROR_LIMIT`],
/// each cut to [`ERROR_TEXT_LIMIT`] bytes, and a count of the rest.
fn record_error(shared: &Shared, message: &str) {
    let mut message = message.to_string();
    if message.len() > ERROR_TEXT_LIMIT {
        let mut cut = ERROR_TEXT_LIMIT;
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        message.truncate(cut);
        message.push_str("...");
    }
    if let Ok(mut errors) = shared.errors.lock() {
        if errors.len() < ERROR_LIMIT {
            errors.push(message);
        } else {
            shared.errors_dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn serve_one(mut sock: TcpStream, shared: &Shared) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(30)));
    let request = read_request(&mut sock);
    let (status, body) = match request.and_then(|(method, path, body)| route(&method, &path, &body, shared)) {
        Ok(body) => (200, body.to_string()),
        Err(error) => {
            let status = error.downcast_ref::<Refusal>().map_or(500, |r| r.0);
            let kind = match status {
                404 => "not_found",
                413 | 431 => "too_large",
                _ => "internal",
            };
            let message = format!("{error:#}");
            record_error(shared, &message);
            (status, json!({"error": {"type": kind, "message": message}}).to_string())
        }
    };
    let out = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(out.as_bytes());
    let _ = sock.shutdown(std::net::Shutdown::Both);
}

fn route(method: &str, path: &str, body: &str, shared: &Shared) -> Result<Value> {
    match (method, path) {
        ("GET", "/council/v2/identity") => Ok(serde_json::to_value(server_identity())?),
        ("POST", "/council/v2/specs") => {
            let value: Value = serde_json::from_str(body).context("a posted spec is not JSON")?;
            let spec = gogi_proto::spec::parse_spec(&value, &SpecRules::default()).map_err(|e| anyhow::anyhow!("a posted spec is refused: {e}"))?;
            let id = spec.id();
            let held = json!({"spec_id": id, "spec": value});
            let mut specs = shared.specs.lock().map_err(|_| anyhow::anyhow!("the spec table is poisoned"))?;
            if specs.len() >= SPEC_LIMIT && !specs.contains_key(id.as_str()) {
                bail!("the scripted council holds at most {SPEC_LIMIT} specs");
            }
            specs.insert(id.to_string(), spec);
            Ok(held)
        }
        ("POST", "/council/v2/contexts") => {
            let build: ContextBuild = serde_json::from_str(body).context("a context build does not decode")?;
            build.context.check().map_err(|e| anyhow::anyhow!("a context build is refused: {e}"))?;
            server_identity().route(&build.model).map_err(|e| anyhow::anyhow!("a context build is refused: {e}"))?;
            Ok(serde_json::to_value(built(&build.context, build.dry_run.unwrap_or(false))?)?)
        }
        ("POST", "/council/v2/decisions") => {
            let request: DecisionRequest = serde_json::from_str(body).context("a decision request does not decode")?;
            request.validate().map_err(|e| anyhow::anyhow!("a decision request is refused: {e}"))?;
            server_identity().route(&request.model).map_err(|e| anyhow::anyhow!("a decision request is refused: {e}"))?;
            let spec_id = &request.spec_id;
            let spec = shared
                .specs
                .lock()
                .map_err(|_| anyhow::anyhow!("the spec table is poisoned"))?
                .get(spec_id.as_str())
                .cloned()
                .with_context(|| format!("a decision names spec {spec_id}, which was never posted"))?;
            let mut answered = shared.answered.lock().map_err(|_| anyhow::anyhow!("the answer log is poisoned"))?;
            let script = shared.script.lock().map_err(|_| anyhow::anyhow!("the script is poisoned"))?;
            let (script, first) = script.as_ref().context("the scripted council has no script")?;
            let verdict = script.verdict(answered.len() - first);
            let reply = answer(&request, &spec, verdict, script.undo)?;
            answered.push(match script.undo {
                Some(undo) => format!("{} undo={}", verdict.option(), undo.option()),
                None => verdict.option().to_string(),
            });
            Ok(reply)
        }
        _ => Err(Refusal(404, format!("the scripted council does not serve {method} {path}")).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_BUMP: &str = include_str!("../../../assets/defaults/council/shell-bump.json");

    fn spec_value() -> Value {
        serde_json::from_str(SHELL_BUMP).unwrap()
    }

    fn spec() -> Spec {
        gogi_proto::spec::parse_spec(&spec_value(), &SpecRules::default()).unwrap()
    }

    fn context(i: usize) -> Value {
        json!({"messages": [{"role": "user", "content": format!("seat {i}")}]})
    }

    fn request(contexts: usize) -> DecisionRequest {
        let spec_id = spec().id();
        let contexts: Vec<Value> = (0..contexts).map(context).collect();
        serde_json::from_value(json!({
            "model": "fleet-council",
            "case": "touch x",
            "spec_id": spec_id,
            "contexts": contexts,
            "pool": {"method": "loglinear", "weights": "mass"},
        }))
        .unwrap()
    }

    fn pooled_choice(body: &Value, question: &str) -> String {
        body["answers"][question]["choice"].as_str().unwrap().to_string()
    }

    /// Send `raw` and return the status and body of the response.
    fn send_raw(addr: &str, raw: &[u8]) -> (u16, String) {
        let mut sock = TcpStream::connect(addr).unwrap();
        let _ = sock.write_all(raw);
        let mut out = Vec::new();
        let _ = sock.read_to_end(&mut out);
        let out = String::from_utf8_lossy(&out).into_owned();
        let status = out.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
        (status, out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
    }

    fn post(addr: &str, path: &str, body: &str) -> (u16, String) {
        send_raw(addr, format!("POST {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes())
    }

    fn council() -> ScriptedCouncil {
        ScriptedCouncil::start(Script { verdicts: vec![Verdict::Proceed], undo: None }).unwrap()
    }

    #[test]
    fn a_body_over_the_limit_gets_413_and_is_recorded() {
        let council = council();
        let addr = &council.base()["http://".len()..];
        let head = format!("POST /council/v2/specs HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n", BODY_LIMIT + 1);
        let (status, _) = send_raw(addr, head.as_bytes());
        assert_eq!(status, 413);
        let errors = council.errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("larger than"), "{errors:?}");
    }

    #[test]
    fn a_head_over_the_limit_is_refused_and_recorded() {
        let council = council();
        let addr = &council.base()["http://".len()..];
        let head = format!("GET /council/v2/identity HTTP/1.1\r\nx-pad: {}", "a".repeat(HEAD_LIMIT + 1));
        let (status, _) = send_raw(addr, head.as_bytes());
        assert_eq!(status, 431);
        assert_eq!(council.errors().len(), 1, "{:?}", council.errors());
    }

    #[test]
    fn errors_are_kept_short_and_counted_past_the_limit() {
        let council = council();
        let addr = &council.base()["http://".len()..];
        let long = format!("/council/v2/{}", "x".repeat(ERROR_TEXT_LIMIT * 4));
        for _ in 0..ERROR_LIMIT + 5 {
            assert_eq!(post(addr, &long, "").0, 404);
        }
        let errors = council.errors();
        assert_eq!(errors.len(), ERROR_LIMIT + 1, "{errors:?}");
        assert!(errors[..ERROR_LIMIT].iter().all(|e| e.len() <= ERROR_TEXT_LIMIT + 3), "{errors:?}");
        assert_eq!(errors[ERROR_LIMIT], "and 5 more error(s)");
    }

    #[test]
    fn the_server_holds_at_most_spec_limit_specs() {
        let council = council();
        let addr = &council.base()["http://".len()..];
        for n in 0..SPEC_LIMIT {
            let mut spec = spec_value();
            spec["name"] = json!(format!("fleet-spec-{n}"));
            let (status, body) = post(addr, "/council/v2/specs", &spec.to_string());
            assert_eq!(status, 200, "{body}");
        }
        let mut spec = spec_value();
        spec["name"] = json!("one-too-many");
        assert_eq!(post(addr, "/council/v2/specs", &spec.to_string()).0, 500);
        assert!(council.errors().iter().any(|e| e.contains("at most")), "{:?}", council.errors());
    }

    #[test]
    fn verdicts_answer_in_order_and_the_last_repeats() {
        let script = Script { verdicts: vec![Verdict::TryHarder, Verdict::DoLess, Verdict::Proceed], undo: None };
        let seen: Vec<&str> = (0..5).map(|n| script.verdict(n).option()).collect();
        assert_eq!(seen, ["try_harder", "do_less", "proceed", "proceed", "proceed"]);
    }

    #[test]
    fn an_answer_puts_the_verdict_on_top_and_verifies() {
        for verdict in [Verdict::Proceed, Verdict::TryHarder, Verdict::DoLess] {
            let body = answer(&request(2), &spec(), verdict, None).unwrap();
            assert_eq!(pooled_choice(&body, "verdict"), verdict.option());
            let p = body["answers"]["verdict"]["probabilities"][verdict.option()].as_f64().unwrap();
            assert!(p > 0.99, "{verdict:?}: pooled p {p} must clear a 0.98 threshold");
            assert!(body["answers"].get("originals").is_none(), "the rubric stays unanswered, so the verdict decides");
            assert_eq!(body["reads"].as_array().unwrap().len(), 2, "one read per context");
            assert_eq!(body["reads"][1]["context"], 1, "a read names its context by index");
        }
    }

    #[test]
    fn an_undo_read_rides_beside_the_verdict() {
        let body = answer(&request(1), &spec(), Verdict::TryHarder, Some(Undo::Irreversible)).unwrap();
        assert_eq!(pooled_choice(&body, "undo"), "irreversible");
        assert_eq!(pooled_choice(&body, "verdict"), "try_harder");
    }

    #[test]
    fn a_spec_without_the_scripted_question_is_an_error() {
        let mut spec = spec();
        spec.questions.retain(|q| q.id != "undo");
        let error = format!("{:#}", answer(&request(1), &spec, Verdict::Proceed, Some(Undo::Normal)).unwrap_err());
        assert!(error.contains("no \"undo\" question"), "{error}");
    }

    #[test]
    fn a_decision_with_no_context_is_an_error() {
        let error = format!("{:#}", answer(&request(0), &spec(), Verdict::Proceed, None).unwrap_err());
        assert!(error.contains("no council context"), "{error}");
    }

    /// The server end to end over its socket: identity, a spec, a context,
    /// then decisions answered in script order.
    #[test]
    fn the_server_answers_each_decision_with_the_next_verdict() {
        let council = ScriptedCouncil::start(Script { verdicts: vec![Verdict::TryHarder, Verdict::DoLess], undo: None }).unwrap();
        let addr = &council.base()["http://".len()..];
        let call = |method: &str, path: &str, body: &str| -> (u16, Value) {
            let mut sock = TcpStream::connect(addr).unwrap();
            write!(sock, "{method} {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut out = String::new();
            sock.read_to_string(&mut out).unwrap();
            let status = out[9..12].parse().unwrap();
            let body = out.split_once("\r\n\r\n").unwrap().1;
            (status, serde_json::from_str(body).unwrap())
        };
        let (status, id) = call("GET", "/council/v2/identity", "");
        assert_eq!(status, 200, "{id}");
        let id: ServerIdentity = serde_json::from_value(id).unwrap();
        assert_eq!(id.route("fleet-council").unwrap().model, "fleet-council");
        assert!(id.capabilities.is_empty(), "no pins capability, so the kernel sends no pins");
        let (status, held) = call("POST", "/council/v2/specs", SHELL_BUMP);
        assert_eq!(status, 200, "{held}");
        assert_eq!(held["spec_id"], spec().id());
        let build = json!({"model": "fleet-council", "context": context(0)}).to_string();
        let (status, first) = call("POST", "/council/v2/contexts", &build);
        assert_eq!(status, 200, "{first}");
        let first: ContextBuilt = serde_json::from_value(first).unwrap();
        let again: ContextBuilt = serde_json::from_value(call("POST", "/council/v2/contexts", &build).1).unwrap();
        assert_eq!(first.head, again.head, "the same context has the same head");
        let other = json!({"model": "fleet-council", "context": context(1), "dry_run": true}).to_string();
        let other: ContextBuilt = serde_json::from_value(call("POST", "/council/v2/contexts", &other).1).unwrap();
        assert_ne!(first.head, other.head);
        assert!(other.dry_run);
        let decision = serde_json::to_string(&request(1)).unwrap();
        let (status, reply) = call("POST", "/council/v2/decisions", &decision);
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["reads"][0]["snapshot"], first.head.as_str(), "a read reports its context's head");
        let verdicts: Vec<String> = std::iter::once(pooled_choice(&reply, "verdict"))
            .chain((0..2).map(|_| pooled_choice(&call("POST", "/council/v2/decisions", &decision).1, "verdict")))
            .collect();
        assert_eq!(verdicts, ["try_harder", "do_less", "do_less"]);
        assert_eq!(council.answered(), ["try_harder", "do_less", "do_less"]);
        council.restart(vec![Verdict::Proceed, Verdict::TryHarder]).unwrap();
        let verdicts: Vec<String> =
            (0..3).map(|_| pooled_choice(&call("POST", "/council/v2/decisions", &decision).1, "verdict")).collect();
        assert_eq!(verdicts, ["proceed", "try_harder", "try_harder"], "a restart answers from its first verdict");
        assert!(council.errors().is_empty(), "{:?}", council.errors());
        assert_eq!(call("GET", "/council/v1/identity", "").0, 404, "council v1 is gone");
        assert_eq!(call("POST", "/council/v2/nowhere", "").0, 404);
        assert_eq!(council.errors().len(), 2, "each unserved request is recorded: {:?}", council.errors());
    }
}
