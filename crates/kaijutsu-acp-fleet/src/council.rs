//! A scripted council server for one scenario run (`docs/acp-fleet.md`,
//! "Council scenarios").
//!
//! It serves the council contract on 127.0.0.1 well enough for a kernel's
//! gate: it reports an identity, holds the specs and contexts it is sent,
//! and answers each decision with the scenario's next verdict. Every answer
//! carries numbers `math::verify` recomputes, so the kernel reads it as a
//! real council's. One verdict answers one decision, in order, and the last
//! one repeats.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kaijutsu_mk::council::math::{self, Row, WeightSpec};
use kaijutsu_mk::council::wire::{DecisionRequest, DecisionResponse, PoolMethod, PoolWeights, Spec, SpecQuestion};
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

/// The template the scripted council reports in its identity and specs.
const TEMPLATE: &str = "mk-letters-1:0123456789abcdef";

fn identity() -> Value {
    json!({"model": "fleet-council", "weight_hash": "fleet", "tokenizer_hash": "fleet", "template": TEMPLATE,
           "engine": "acp-fleet"})
}

fn server_identity() -> Value {
    let mut id = identity();
    id["limits"] = json!({"context_tokens": 100000, "state_bytes": 100000, "contexts_per_decision": 8,
                          "choice_options": 8, "default_timeout_ms": 1000});
    id["capabilities"] = json!([]);
    id
}

/// The options of `spec`'s choice question `id`, in the spec's order.
fn choice_options<'a>(spec: &'a Spec, id: &str) -> Result<Vec<&'a str>> {
    match spec.questions.iter().find(|q| q.id() == id) {
        Some(SpecQuestion::Choice(q)) => Ok(q.criteria.iter().map(|c| c.option.as_str()).collect()),
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
    let contexts = request.contexts.clone().unwrap_or_default();
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
        .map(|(c, answers)| {
            json!({
                "context": c.id,
                "snapshot": c.at.as_ref().map(|s| s.to_string()).unwrap_or_else(|| format!("snap:{}", "0".repeat(64))),
                "answers": answers,
                "rendered_sha256": "0".repeat(64),
            })
        })
        .collect();
    let mut id = identity();
    id["spec_id"] = json!(request.spec_id);
    let body = json!({
        "model": "fleet-council",
        "answers": pooled_answers,
        "reads": reads,
        "pool": {"method": method, "weights": weights, "normalized": normalized},
        "signals": {"control_text": []},
        "identity": id,
        "queue_ms": 1.0,
        "ms": 1.0,
    });
    let decoded: DecisionResponse = serde_json::from_value(body.clone()).context("the scripted answer does not decode")?;
    math::verify(&decoded, request).map_err(|m| anyhow::anyhow!("the scripted answer fails math::verify: {m}"))?;
    Ok(body)
}

/// What the server has seen, shared with its connection threads.
#[derive(Default)]
struct Shared {
    /// Specs posted to it, by spec id.
    specs: Mutex<HashMap<String, Spec>>,
    /// Each decision's answer, in order: the verdict, and the `undo` read.
    answered: Mutex<Vec<String>>,
    /// Requests it could not serve, each named.
    errors: Mutex<Vec<String>>,
    puts: AtomicU64,
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
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let script = Arc::new(script);
        let (state, stopping) = (shared.clone(), stop.clone());
        let accept = std::thread::spawn(move || {
            for sock in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(sock) = sock else { continue };
                let (state, script) = (state.clone(), script.clone());
                std::thread::spawn(move || serve_one(sock, &state, &script));
            }
        });
        Ok(Self { base: format!("http://{addr}"), shared, stop, accept: Some(accept) })
    }

    /// The server's address, as `[council] server` takes it.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Each decision's answer so far, in order, such as `try_harder` or
    /// `do_less undo=irreversible`.
    pub fn answered(&self) -> Vec<String> {
        self.shared.answered.lock().map(|a| a.clone()).unwrap_or_default()
    }

    /// Every request the server could not serve.
    pub fn errors(&self) -> Vec<String> {
        self.shared.errors.lock().map(|e| e.clone()).unwrap_or_default()
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

/// One HTTP request: method, path, body.
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
            break (i + 4, len);
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
    Ok((method, path, String::from_utf8_lossy(&buf[head_end..]).to_string()))
}

fn serve_one(mut sock: TcpStream, shared: &Shared, script: &Script) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(30)));
    let request = read_request(&mut sock);
    let (status, body) = match request.and_then(|(method, path, body)| route(&method, &path, &body, shared, script)) {
        Ok(body) => (200, body.to_string()),
        Err(error) => {
            let message = format!("{error:#}");
            if let Ok(mut errors) = shared.errors.lock() {
                errors.push(message.clone());
            }
            (500, json!({"error": {"type": "internal", "message": message}}).to_string())
        }
    };
    let out = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(out.as_bytes());
    let _ = sock.shutdown(std::net::Shutdown::Both);
}

fn route(method: &str, path: &str, body: &str, shared: &Shared, script: &Script) -> Result<Value> {
    match (method, path) {
        ("GET", "/council/v1/identity") => Ok(server_identity()),
        ("POST", "/council/v1/specs") => {
            let spec: Spec = serde_json::from_str(body).context("a posted spec does not decode")?;
            let id = kaijutsu_mk::council::canon::spec_id(&spec).context("a posted spec has no canonical id")?;
            let held = json!({"spec_id": id, "spec": spec, "template": TEMPLATE});
            shared.specs.lock().map_err(|_| anyhow::anyhow!("the spec table is poisoned"))?.insert(id.to_string(), spec);
            Ok(held)
        }
        ("PUT", p) if p.starts_with("/council/v1/contexts/") => {
            let n = shared.puts.fetch_add(1, Ordering::SeqCst) + 1;
            let id = p.rsplit('/').next().unwrap_or("");
            Ok(json!({"id": id, "head": format!("snap:{n:064x}"), "tokens": 1, "kept": 0, "fed": 1,
                      "dry_run": false, "snapshots": []}))
        }
        ("POST", "/council/v1/decisions") => {
            let request: DecisionRequest = serde_json::from_str(body).context("a decision request does not decode")?;
            let spec_id = request.spec_id.as_ref().context("a decision with no spec_id; the scripted council answers specs only")?;
            let spec = shared
                .specs
                .lock()
                .map_err(|_| anyhow::anyhow!("the spec table is poisoned"))?
                .get(spec_id.as_str())
                .cloned()
                .with_context(|| format!("a decision names spec {spec_id}, which was never posted"))?;
            let mut answered = shared.answered.lock().map_err(|_| anyhow::anyhow!("the answer log is poisoned"))?;
            let verdict = script.verdict(answered.len());
            let reply = answer(&request, &spec, verdict, script.undo)?;
            answered.push(match script.undo {
                Some(undo) => format!("{} undo={}", verdict.option(), undo.option()),
                None => verdict.option().to_string(),
            });
            Ok(reply)
        }
        _ => bail!("the scripted council does not serve {method} {path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_BUMP: &str = include_str!("../../../assets/defaults/council/shell-bump.json");

    fn spec() -> Spec {
        serde_json::from_str(SHELL_BUMP).unwrap()
    }

    fn request(contexts: usize) -> DecisionRequest {
        let spec_id = kaijutsu_mk::council::canon::spec_id(&spec()).unwrap();
        let contexts: Vec<Value> = (0..contexts)
            .map(|i| json!({"id": format!("0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b1{i}"), "at": format!("snap:{:064x}", i + 1)}))
            .collect();
        serde_json::from_value(json!({
            "state": "touch x",
            "spec_id": spec_id,
            "contexts": contexts,
            "pool": {"method": "loglinear", "weights": "mass"},
        }))
        .unwrap()
    }

    fn pooled_choice(body: &Value, question: &str) -> String {
        body["answers"][question]["choice"].as_str().unwrap().to_string()
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
        spec.questions.retain(|q| q.id() != "undo");
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
        assert_eq!(call("GET", "/council/v1/identity", "").1["model"], "fleet-council");
        let (status, held) = call("POST", "/council/v1/specs", SHELL_BUMP);
        assert_eq!(status, 200, "{held}");
        assert_eq!(call("PUT", "/council/v1/contexts/0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b10", "{}").0, 200);
        let decision = serde_json::to_string(&request(1)).unwrap();
        let verdicts: Vec<String> =
            (0..3).map(|_| pooled_choice(&call("POST", "/council/v1/decisions", &decision).1, "verdict")).collect();
        assert_eq!(verdicts, ["try_harder", "do_less", "do_less"]);
        assert_eq!(council.answered(), ["try_harder", "do_less", "do_less"]);
        assert!(council.errors().is_empty(), "{:?}", council.errors());
        let (status, _) = call("POST", "/council/v1/nowhere", "");
        assert_eq!(status, 500);
        assert_eq!(council.errors().len(), 1, "an unserved request is recorded");
    }
}
