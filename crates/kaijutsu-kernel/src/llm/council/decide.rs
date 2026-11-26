//! A turn taken as council decisions.
//!
//! When a turn names a decision program (`BuildOpts::decide`, a JSON file read
//! through the kernel VFS), the reply is assembled from decisions
//! (`POST /council/v2/decisions`), and the server samples no tokens. Each slot
//! of the program is one choice among a set of options. A slot is read in
//! several orderings of its options, so one ordering's position bias does not
//! pick the answer; the orderings' pooled probabilities are sampled with the
//! turn's temperature. The conversation goes to every decision as its one
//! context, the system message and the last `history_messages` others.
//!
//! The program, in the order each slot is decided:
//!
//! ```json
//! {"instructions": "...", "input_label": "...", "question": "...",
//!  "prefix": "", "join": " | ", "suffix": "", "shuffle": 4,
//!  "history_messages": 8, "no_repeat": true,
//!  "options": {"id": {"means": "...", "text": "..."}},
//!  "slots": [{"case": "...", "options": ["id", "id"]}]}
//! ```

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gogi_proto::Context;
use gogi_proto::decision::{DecisionRequest, DecisionResponse, PooledAnswer};
use gogi_proto::identity::{Capability, ServedModel};
use gogi_proto::message::{Message as CouncilMessage, Role as CouncilRole};
use gogi_proto::spec::{Spec, SpecRules, parse_spec};
use serde::Deserialize;
use serde_json::{Value, json};

use super::reply::Reply;
use super::{Client, Stream, Turn, build};
use crate::llm::request_dump::RequestTrace;
use crate::llm::stream::{BuildOpts, CouncilUsageExtra, SpentUsage, StreamError, StreamErrorKind, StreamEvent, UsageExtra};
use crate::llm::{LlmError, LlmResult, Message};

/// A choice answer's probabilities, by option id.
type Probabilities = std::collections::HashMap<String, f64>;

/// The id of the one question each decision asks.
const QUESTION: &str = "next";

/// The orderings one slot is read in, at most.
const MAX_SHUFFLE: i64 = 8;

/// The messages after the system message a decision keeps when the program
/// does not say.
const DEFAULT_HISTORY_MESSAGES: usize = 8;

/// The program file as written. An unknown member is refused, so a misspelled
/// one does not change the program without saying so.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    instructions: String,
    input_label: String,
    question: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    join: String,
    #[serde(default)]
    suffix: String,
    shuffle: Option<i64>,
    history_messages: Option<i64>,
    #[serde(default)]
    no_repeat: bool,
    options: serde_json::Map<String, Value>,
    slots: Vec<RawSlot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    case: String,
    options: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOption {
    means: String,
    text: String,
}

/// One option a slot may pick.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Choice {
    pub(super) id: String,
    pub(super) means: String,
    pub(super) text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Slot {
    pub(super) case: String,
    /// Indexes into [`Program::options`], in the order the slot lists them.
    pub(super) options: Vec<usize>,
}

/// A decision program that parsed and checked.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Program {
    pub(super) path: String,
    pub(super) instructions: String,
    pub(super) input_label: String,
    pub(super) question: String,
    pub(super) prefix: String,
    pub(super) join: String,
    pub(super) suffix: String,
    pub(super) shuffle: usize,
    pub(super) history_messages: usize,
    pub(super) no_repeat: bool,
    /// In the order the file lists them.
    pub(super) options: Vec<Choice>,
    pub(super) slots: Vec<Slot>,
}

impl Program {
    /// Parses and checks the program in `text`, read from `path`. A refusal
    /// names the program's path and the field.
    pub(super) fn parse(backend: &str, path: &str, text: &str) -> LlmResult<Program> {
        let refuse = |field: &str, why: String| {
            LlmError::InvalidRequest(format!("council backend '{backend}': decision program {path}: {field}: {why}"))
        };
        let raw: Raw = serde_json::from_str(text).map_err(|e| refuse("(file)", format!("is not a decision program: {e}")))?;
        let shuffle = raw.shuffle.unwrap_or(1);
        if !(1..=MAX_SHUFFLE).contains(&shuffle) {
            return Err(refuse("shuffle", format!("is {shuffle}; it is the number of orderings, 1 to {MAX_SHUFFLE}")));
        }
        let history_messages = match raw.history_messages {
            None => DEFAULT_HISTORY_MESSAGES,
            Some(n) if n >= 1 => n as usize,
            Some(n) => return Err(refuse("history_messages", format!("is {n}; it keeps at least 1 message"))),
        };
        if raw.slots.is_empty() {
            return Err(refuse("slots", "is empty; a program decides at least one slot".into()));
        }
        let mut options = Vec::with_capacity(raw.options.len());
        for (id, value) in raw.options {
            let RawOption { means, text } = serde_json::from_value(value)
                .map_err(|e| refuse(&format!("options.{id}"), format!("is not {{\"means\": ..., \"text\": ...}}: {e}")))?;
            options.push(Choice { id, means, text });
        }
        let mut slots = Vec::with_capacity(raw.slots.len());
        for (i, slot) in raw.slots.into_iter().enumerate() {
            let field = format!("slots[{i}].options");
            let listed = match slot.options {
                None => (0..options.len()).collect(),
                Some(ids) => {
                    let mut listed = Vec::with_capacity(ids.len());
                    for id in &ids {
                        let Some(at) = options.iter().position(|o| &o.id == id) else {
                            let known: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
                            return Err(refuse(&field, format!("names option {id:?}, which the program's options lack; it has {}", known.join(", "))));
                        };
                        if listed.contains(&at) {
                            return Err(refuse(&field, format!("names option {id:?} twice")));
                        }
                        listed.push(at);
                    }
                    listed
                }
            };
            if listed.len() < 2 {
                return Err(refuse(&field, format!("lists {} option(s); a slot chooses among at least 2", listed.len())));
            }
            slots.push(Slot { case: slot.case, options: listed });
        }
        Ok(Program {
            path: path.to_string(),
            instructions: raw.instructions,
            input_label: raw.input_label,
            question: raw.question,
            prefix: raw.prefix,
            join: raw.join,
            suffix: raw.suffix,
            shuffle: shuffle as usize,
            history_messages,
            no_repeat: raw.no_repeat,
            options,
            slots,
        })
    }

    /// The options slot `at` may pick after `previous`, in the order the slot
    /// lists them. With `no_repeat`, the previous pick is left out unless that
    /// would leave fewer than 2.
    pub(super) fn allowed(&self, at: usize, previous: Option<usize>) -> Vec<usize> {
        let listed = &self.slots[at].options;
        match previous {
            Some(prev) if self.no_repeat => {
                let without: Vec<usize> = listed.iter().copied().filter(|o| *o != prev).collect();
                if without.len() >= 2 { without } else { listed.clone() }
            }
            _ => listed.clone(),
        }
    }

    /// The reply: prefix, the picks' text joined, suffix.
    pub(super) fn reply_text(&self, picks: &[usize]) -> String {
        format!("{}{}{}", self.prefix, self.picked(picks), self.suffix)
    }

    /// The picks' text joined, with no prefix or suffix.
    fn picked(&self, picks: &[usize]) -> String {
        picks.iter().map(|p| self.options[*p].text.as_str()).collect::<Vec<_>>().join(&self.join)
    }
}

/// A small deterministic generator (splitmix64), so a turn's orderings and
/// sample replay from the seed it reports.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// A seed for a turn: the clock mixed with a counter, so turns started in
    /// the same instant differ.
    pub(super) fn fresh_seed() -> u64 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Rng(nanos ^ n.rotate_left(32)).next_u64()
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// `count` orderings of `options` (capped at `options.len()!`). Ordering 0 is
/// `options` as listed; when `options.len()! <= count` every permutation
/// appears once, and otherwise the remaining orderings are distinct seeded
/// permutations.
pub(super) fn orderings(options: &[usize], count: usize, rng: &mut Rng) -> Vec<Vec<usize>> {
    if count == 0 {
        return Vec::new();
    }
    let fact = (1..=options.len()).fold(1usize, |a, b| a.saturating_mul(b));
    let target = fact.min(count);
    if target == fact {
        let mut out = Vec::with_capacity(fact);
        let mut perm: Vec<usize> = (0..options.len()).collect();
        loop {
            out.push(perm.iter().map(|&i| options[i]).collect());
            if !next_permutation(&mut perm) {
                break;
            }
        }
        return out;
    }
    let mut out = Vec::with_capacity(target);
    out.push(options.to_vec());
    while out.len() < target {
        let mut candidate = options.to_vec();
        let mut found = false;
        for _ in 0..16 {
            for i in (1..candidate.len()).rev() {
                candidate.swap(i, rng.below(i + 1));
            }
            if !out.contains(&candidate) {
                found = true;
                break;
            }
        }
        if !found {
            let mut perm: Vec<usize> = (0..options.len()).collect();
            loop {
                let next: Vec<usize> = perm.iter().map(|&i| options[i]).collect();
                if !out.contains(&next) {
                    candidate = next;
                    break;
                }
                if !next_permutation(&mut perm) {
                    break;
                }
            }
        }
        out.push(candidate);
    }
    out
}

fn next_permutation(slice: &mut [usize]) -> bool {
    let Some(i) = (0..slice.len().saturating_sub(1)).rfind(|&i| slice[i] < slice[i + 1]) else {
        return false;
    };
    let j = (i + 1..slice.len()).rfind(|&j| slice[j] > slice[i]).unwrap();
    slice.swap(i, j);
    slice[i + 1..].reverse();
    true
}

/// The mean over `answers` of each option's probability, in `options` order.
/// An answer that lacks an option, or whose probability is not a number in
/// `[0, 1]`, is a refusal naming the option.
pub(super) fn pool(options: &[&str], answers: &[Probabilities]) -> Result<Vec<f64>, String> {
    let mut pooled = vec![0.0; options.len()];
    for answer in answers {
        for (slot, id) in pooled.iter_mut().zip(options) {
            let p = answer.get(*id).copied().ok_or_else(|| format!("an answer has no probability for option {id:?}"))?;
            if !(0.0..=1.0 + 1e-9).contains(&p) {
                return Err(format!("an answer gives option {id:?} the probability {p}, which is not in 0 to 1"));
            }
            *slot += p;
        }
    }
    for slot in &mut pooled {
        *slot /= answers.len() as f64;
    }
    Ok(pooled)
}

/// The index sampled from `probabilities` with `temperature`: weights
/// `p^(1/T)`, `T` absent as 1. `T == 0` is the argmax, ties to the earlier.
pub(super) fn sample(probabilities: &[f64], temperature: Option<f64>, rng: &mut Rng) -> Result<usize, String> {
    let t = temperature.unwrap_or(1.0);
    if !t.is_finite() || t < 0.0 {
        return Err(format!("temperature {t} is not a number at least 0"));
    }
    let mut best = 0;
    for (i, p) in probabilities.iter().enumerate() {
        if *p > probabilities[best] {
            best = i;
        }
    }
    if probabilities[best] <= 0.0 {
        return Err("every option has probability 0".into());
    }
    if t == 0.0 {
        return Ok(best);
    }
    // Relative to the largest, so a small `T` does not underflow every weight.
    let top = probabilities[best].ln();
    let weights: Vec<f64> = probabilities.iter().map(|p| if *p > 0.0 { ((p.ln() - top) / t).exp() } else { 0.0 }).collect();
    let total: f64 = weights.iter().sum();
    let mut draw = rng.unit() * total;
    for (i, w) in weights.iter().enumerate() {
        if draw < *w {
            return Ok(i);
        }
        draw -= w;
    }
    Ok(best)
}

/// The spec for one ordering of `options`: one `choice` question.
fn spec_for(program: &Program, ordering: &[usize], choice_options: usize) -> LlmResult<Spec> {
    let criteria: Vec<Value> = ordering
        .iter()
        .map(|o| {
            let c = &program.options[*o];
            json!({"option": c.id, "means": format!("{}: {}", c.means, c.text)})
        })
        .collect();
    let value = json!({
        "name": "decision-turn",
        "instructions": program.instructions,
        "input_label": program.input_label,
        "questions": [{"id": QUESTION, "type": "choice", "instructions": program.question, "criteria": criteria}],
    });
    parse_spec(&value, &SpecRules { choice_options, ..SpecRules::default() }).map_err(|e| {
        LlmError::InvalidRequest(format!(
            "decision program {}: the spec for options {} is refused: {e:?}",
            program.path,
            ordering.iter().map(|o| program.options[*o].id.as_str()).collect::<Vec<_>>().join(", ")
        ))
    })
}

/// `messages` with the system message first kept, and of the rest only the
/// last `keep`. A tool result that would lead the kept messages is dropped, as
/// the call it answers is gone.
pub(super) fn trim(messages: Vec<CouncilMessage>, has_system: bool, keep: usize) -> Vec<CouncilMessage> {
    let split = usize::from(has_system && messages.first().is_some_and(|m| m.role == CouncilRole::System));
    let mut head = messages;
    let rest = head.split_off(split);
    let from = rest.len().saturating_sub(keep);
    let mut kept: Vec<CouncilMessage> = rest.into_iter().skip(from).collect();
    let lead = kept.iter().take_while(|m| m.role == CouncilRole::Tool).count();
    kept.drain(..lead);
    head.extend(kept);
    head
}

/// The mean probabilities of `response`'s pooled `next` answer, by option id.
fn probabilities(response: &DecisionResponse) -> Result<Probabilities, String> {
    match response.answers.get(QUESTION) {
        Some(PooledAnswer::Choice(c)) => Ok(c.probabilities.iter().map(|(k, v)| (k.clone(), *v)).collect()),
        Some(_) => Err(format!("the answer to question {QUESTION:?} is not a choice")),
        None => Err(format!("the response has no answer to question {QUESTION:?}")),
    }
}

impl Client {
    /// A streamed turn assembled from decisions: the program `opts.decide`
    /// names is read through `opts.vfs`, the served model must list `reads`,
    /// and each slot is decided in `program.shuffle` orderings. Every refusal
    /// comes before the first decision; a failed decision fails the turn.
    pub(super) async fn stream_decisions(&self, path: &str, opts: BuildOpts, history: Vec<Message>, turn: Turn) -> LlmResult<Stream> {
        let program = self.read_program(path, &opts).await?;
        self.stream_decisions_seeded(program, opts, history, turn, Rng::fresh_seed()).await
    }

    async fn read_program(&self, path: &str, opts: &BuildOpts) -> LlmResult<Program> {
        use crate::vfs::VfsOps as _;
        let Some(vfs) = &opts.vfs else {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': decision program {path}: this turn carries no VFS to read it through",
                self.name
            )));
        };
        let bytes = vfs.read_all(Path::new(path)).await.map_err(|e| {
            LlmError::InvalidRequest(format!(
                "council backend '{}': decision program {path}: cannot be read: {e}; \
                 a shipped program is restored with `kaijutsu-server rc reseed`",
                self.name
            ))
        })?;
        let text = String::from_utf8(bytes).map_err(|_| {
            LlmError::InvalidRequest(format!("council backend '{}': decision program {path}: is not UTF-8 text", self.name))
        })?;
        Program::parse(&self.name, path, &text)
    }

    /// [`Self::stream_decisions`] with the seed the orderings and the sample
    /// replay from.
    pub(super) async fn stream_decisions_seeded(
        &self,
        program: Program,
        opts: BuildOpts,
        history: Vec<Message>,
        turn: Turn,
        seed: u64,
    ) -> LlmResult<Stream> {
        let started = Instant::now();
        let (served, choice_options, holds_objects) = self.served_for_decisions(&turn, &opts.model, &program).await?;
        super::generation::check_controls(&self.name, &opts.controls, &served)?;
        for (i, slot) in program.slots.iter().enumerate() {
            if slot.options.len() as u64 > choice_options {
                return Err(LlmError::InvalidRequest(format!(
                    "council backend '{}': decision program {}: slots[{i}].options: lists {} options, and the council \
                     server takes at most {choice_options} in a choice (limits.choice_options)",
                    self.name,
                    program.path,
                    slot.options.len()
                )));
            }
        }
        let objects = self.place_media(&turn, &history, &served, holds_objects).await?;
        let whole = build::messages(opts.system.as_deref(), &history, &objects)?;
        let has_system = opts.system.is_some();
        // The kept history shrinks while the server answers that the context is
        // too large for a decision, so `keep` is what the first slot found to fit.
        let mut keep = program.history_messages;
        let messages = trim(whole.clone(), has_system, keep);
        keep = keep.min(messages.len().saturating_sub(usize::from(has_system)));
        if messages.is_empty() {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': decision program {}: the conversation is empty, so there is nothing to decide after",
                self.name, program.path
            )));
        }
        let context = Context { thinking: false, tools: Vec::new(), queue: None, control: opts.controls.clone(), messages };
        let runner = DecisionRunner {
            client: self.clone(),
            turn,
            program,
            opts,
            history,
            served,
            choice_options,
            whole,
            has_system,
            keep,
            context,
            rng: Rng::new(seed),
            seed,
            slot_idx: 0,
            picks: Vec::new(),
            usage: DecisionUsage::default(),
            dump_seq: AtomicU64::new(0),
        };
        Ok(Stream {
            name: self.name.clone(),
            inner: None,
            reply: Some(Reply::decisions(runner)),
            pending: Default::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
            finished: false,
            started,
            first_token: None,
            slot: None,
            reply_identity: None,
        })
    }

    fn program_allowed(&self, program: &Program, at: usize, picks: &[usize]) -> Vec<usize> {
        program.allowed(at, picks.last().copied())
    }

    fn refused_answer(&self, program: &Program, at: usize, why: String) -> LlmError {
        LlmError::InvalidRequest(format!(
            "council backend '{}': decision program {}: slots[{at}]: {why}",
            self.name, program.path
        ))
    }

    /// The served model `model` names, which must list `reads`, with the
    /// server's `choice_options` limit and whether it holds objects.
    async fn served_for_decisions(&self, turn: &Turn, model: &str, program: &Program) -> LlmResult<(ServedModel, u64, bool)> {
        let identity = self.read_identity(turn).await?;
        let served = super::generation::served(&self.name, &identity, model, Capability::Reads)?;
        if !served.has(Capability::Reads) {
            return Err(LlmError::InvalidRequest(format!(
                "council backend '{}': decision program {}: served model {} does not list reads, so it cannot decide; \
                 the server serves {}. Name a model that lists reads",
                self.name,
                program.path,
                served.model,
                super::generation::served_list(&identity, Capability::Reads)
            )));
        }
        Ok((served.clone(), identity.limits.choice_options, identity.has(Capability::Objects)))
    }

    /// One decision. The spec is posted first when this client has not posted
    /// it to the server; a `404` for the spec posts it and sends the decision
    /// once more.
    #[allow(clippy::too_many_arguments)]
    async fn decide(
        &self,
        turn: &Turn,
        spec: &Spec,
        request: &DecisionRequest,
        too_large: &AtomicBool,
        trace: Option<&RequestTrace>,
        dump_seq: &AtomicU64,
        model: &str,
        system: Option<&str>,
        history: &[Message],
    ) -> Result<DecisionResponse, (StreamErrorKind, LlmError)> {
        self.ensure_spec(turn, spec).await?;
        let missing = AtomicBool::new(false);
        match self.decide_once(turn, request, &missing, too_large, trace, dump_seq, model, system, history).await {
            Err(_) if missing.load(Ordering::Relaxed) => {
                self.specs.lock().remove(&request.spec_id);
                self.ensure_spec(turn, spec).await?;
                self.decide_once(turn, request, &AtomicBool::new(false), too_large, trace, dump_seq, model, system, history).await
            }
            answered => answered,
        }
    }

    /// One `POST /council/v2/decisions` through the endpoint. `missing` is set
    /// when the server answers 404, which says it does not hold the spec, and
    /// `too_large` when it answers 413, which says a context is past the
    /// decision limit.
    #[allow(clippy::too_many_arguments)]
    async fn decide_once(
        &self,
        turn: &Turn,
        request: &DecisionRequest,
        missing: &AtomicBool,
        too_large: &AtomicBool,
        trace: Option<&RequestTrace>,
        dump_seq: &AtomicU64,
        model: &str,
        system: Option<&str>,
        history: &[Message],
    ) -> Result<DecisionResponse, (StreamErrorKind, LlmError)> {
        let (traceparent, _) = kaijutsu_telemetry::inject_trace_context();
        let traceparent = (!traceparent.is_empty()).then_some(traceparent);
        let seq = dump_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut recorded = false;
        let (_, response) = self
            .call_classified(turn, false, |left: Duration| {
                if !std::mem::replace(&mut recorded, true) {
                    crate::llm::request_dump::record_decision(
                        trace, seq, &self.name, model, &self.base_url, system, history, request,
                    );
                }
                let traceparent = traceparent.as_deref();
                async move {
                    let answered = self.client.decide_traced(request, left, traceparent).await;
                    if answered.as_ref().is_err_and(|e| e.status() == Some(404)) {
                        missing.store(true, Ordering::Relaxed);
                    }
                    if answered.as_ref().is_err_and(|e| e.status() == Some(413)) {
                        too_large.store(true, Ordering::Relaxed);
                    }
                    answered
                }
            })
            .await?;
        Ok(response)
    }

    /// Posts `spec` unless this client has posted it to the server.
    async fn ensure_spec(&self, turn: &Turn, spec: &Spec) -> Result<(), (StreamErrorKind, LlmError)> {
        let id = spec.id();
        if self.specs.lock().contains(&id) {
            return Ok(());
        }
        self.call_classified(turn, false, |_| self.client.post_spec(spec)).await?;
        self.specs.lock().insert(id);
        Ok(())
    }
}

#[derive(Default)]
struct DecisionUsage {
    decisions: usize,
    prompt: u64,
    completion: u64,
    kept: u64,
    fed: u64,
}

impl DecisionUsage {
    fn record(&mut self, response: &DecisionResponse) {
        self.decisions += 1;
        self.prompt += response.usage.prompt;
        self.completion += response.usage.completion;
        self.kept += response.usage.kept;
        self.fed += response.usage.fed;
    }

    fn council_extra(&self, wall_ms: u64, seed: u64, served: &ServedModel) -> CouncilUsageExtra {
        CouncilUsageExtra {
            kept: self.kept,
            fed: self.fed,
            first_token_ms: wall_ms,
            wall_ms,
            seed: Some(seed),
            model: served.model.clone(),
            weight_hash: served.weight_hash.clone(),
        }
    }

    fn spent_usage(&self, wall_ms: u64, seed: u64, served: &ServedModel) -> Option<SpentUsage> {
        (self.decisions > 0).then(|| SpentUsage {
            input_tokens: Some(self.prompt),
            output_tokens: Some(self.completion),
            extra: Some(UsageExtra::Council(self.council_extra(wall_ms, seed, served))),
        })
    }
}

pub(super) enum StepOutcome {
    Continue,
    Finished(Vec<StreamEvent>),
    Failed(StreamError),
}

pub(super) struct DecisionRunner {
    client: Client,
    turn: Turn,
    program: Program,
    opts: BuildOpts,
    history: Vec<Message>,
    served: ServedModel,
    choice_options: u64,
    whole: Vec<CouncilMessage>,
    has_system: bool,
    keep: usize,
    context: Context,
    rng: Rng,
    seed: u64,
    slot_idx: usize,
    picks: Vec<usize>,
    usage: DecisionUsage,
    dump_seq: AtomicU64,
}

impl DecisionRunner {
    fn error_with_usage(&self, kind: StreamErrorKind, message: String, started: Instant) -> StreamError {
        let mut err = StreamError::new(kind, message);
        let wall_ms = started.elapsed().as_millis() as u64;
        if let Some(spent) = self.usage.spent_usage(wall_ms, self.seed, &self.served) {
            err = err.with_usage(spent);
        }
        err
    }

    pub(super) fn cancel_done(self, started: Instant) -> StreamEvent {
        let wall_ms = started.elapsed().as_millis() as u64;
        match self.usage.spent_usage(wall_ms, self.seed, &self.served) {
            Some(spent) => StreamEvent::Done {
                stop_reason: None,
                input_tokens: spent.input_tokens,
                output_tokens: spent.output_tokens,
                extra: spent.extra,
            },
            None => StreamEvent::Done {
                stop_reason: None,
                input_tokens: None,
                output_tokens: None,
                extra: None,
            },
        }
    }

    pub(super) async fn step(&mut self, started: Instant) -> StepOutcome {
        if self.slot_idx < self.program.slots.len() {
            let at = self.slot_idx;
            let allowed = self.client.program_allowed(&self.program, at, &self.picks);
            let so_far = if self.picks.is_empty() {
                "(nothing yet)".to_string()
            } else {
                self.program.picked(&self.picks)
            };
            let case = format!("{} So far: {so_far}", self.program.slots[at].case);
            let answers = loop {
                let mut asks = Vec::new();
                for ordering in orderings(&allowed, self.program.shuffle, &mut self.rng) {
                    let spec = match spec_for(&self.program, &ordering, self.choice_options as usize) {
                        Ok(s) => s,
                        Err(e) => {
                            return StepOutcome::Failed(self.error_with_usage(
                                StreamErrorKind::InvalidRequest,
                                e.to_string(),
                                started,
                            ));
                        }
                    };
                    let request = DecisionRequest {
                        model: self.served.model.clone(),
                        case: Value::String(case.clone()),
                        spec_id: spec.id(),
                        contexts: vec![self.context.clone()],
                        ..Default::default()
                    };
                    asks.push((spec, request));
                }
                let too_large = AtomicBool::new(false);
                let mut results: Vec<Option<Result<DecisionResponse, (StreamErrorKind, LlmError)>>> =
                    (0..asks.len()).map(|_| None).collect();
                {
                    let client = &self.client;
                    let turn = &self.turn;
                    let trace = self.opts.trace.as_deref();
                    let dump_seq = &self.dump_seq;
                    let model = &self.opts.model;
                    let system = self.opts.system.as_deref();
                    let history = &self.history;
                    let mut futs: futures::stream::FuturesUnordered<_> = asks
                        .iter()
                        .enumerate()
                        .map(|(idx, (spec, request))| {
                            let too_large = &too_large;
                            async move {
                                let res = client
                                    .decide(turn, spec, request, too_large, trace, dump_seq, model, system, history)
                                    .await;
                                (idx, res)
                            }
                        })
                        .collect();
                    while let Some((idx, res)) = futs.next().await {
                        if let Ok(ref response) = res {
                            self.usage.record(response);
                        }
                        results[idx] = Some(res);
                    }
                }
                if too_large.load(Ordering::Relaxed) && self.keep > 1 {
                    self.keep -= 1;
                    self.context.messages = trim(self.whole.clone(), self.has_system, self.keep);
                    continue;
                }
                let mut answers = Vec::with_capacity(results.len());
                for item in results {
                    match item.expect("every ask completed") {
                        Ok(resp) => answers.push(resp),
                        Err((kind, err)) => {
                            return StepOutcome::Failed(self.error_with_usage(kind, err.to_string(), started));
                        }
                    }
                }
                break answers;
            };
            let mut each = Vec::with_capacity(answers.len());
            for response in &answers {
                match probabilities(response) {
                    Ok(p) => each.push(p),
                    Err(e) => {
                        let err = self.client.refused_answer(&self.program, at, e);
                        return StepOutcome::Failed(self.error_with_usage(
                            StreamErrorKind::InvalidRequest,
                            err.to_string(),
                            started,
                        ));
                    }
                }
            }
            let ids: Vec<&str> = allowed.iter().map(|o| self.program.options[*o].id.as_str()).collect();
            let pooled = match pool(&ids, &each) {
                Ok(p) => p,
                Err(e) => {
                    let err = self.client.refused_answer(&self.program, at, e);
                    return StepOutcome::Failed(self.error_with_usage(
                        StreamErrorKind::InvalidRequest,
                        err.to_string(),
                        started,
                    ));
                }
            };
            let chosen = match sample(&pooled, self.opts.temperature, &mut self.rng) {
                Ok(c) => c,
                Err(e) => {
                    let err = self.client.refused_answer(&self.program, at, e);
                    return StepOutcome::Failed(self.error_with_usage(
                        StreamErrorKind::InvalidRequest,
                        err.to_string(),
                        started,
                    ));
                }
            };
            self.picks.push(allowed[chosen]);
            self.slot_idx += 1;
            if self.slot_idx < self.program.slots.len() {
                return StepOutcome::Continue;
            }
        }
        let text = self.program.reply_text(&self.picks);
        let wall_ms = started.elapsed().as_millis() as u64;
        let extra = self.usage.council_extra(wall_ms, self.seed, &self.served);
        match Reply::whole(&text, (self.usage.prompt, self.usage.completion, extra)) {
            Ok(events) => StepOutcome::Finished(events),
            Err(e) => StepOutcome::Failed(self.error_with_usage(
                StreamErrorKind::Protocol,
                format!("council backend '{}': {e}", self.client.name),
                started,
            )),
        }
    }
}

/// The specs this client has posted to its server.
pub(super) type PostedSpecs = std::sync::Arc<parking_lot::Mutex<HashSet<String>>>;

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use kaijutsu_council::test_server::{Captured, Reply, reply};

    use super::*;
    use crate::llm::stream::StreamEvent;
    use crate::llm::{ContentBlock, MessageContent, Role};
    use crate::vfs::{MemoryBackend, MountTable, VfsOps};

    const PATH: &str = "/p.json";

    /// A program with the options a, b, c, d and the given `slots`.
    fn program(shuffle: i64, slots: Value) -> Value {
        json!({
            "instructions": "Be the bass.", "input_label": "Phrase", "question": "Next bar?",
            "prefix": "P:", "join": "|", "suffix": "]", "shuffle": shuffle, "history_messages": 2, "no_repeat": true,
            "options": {"a": {"means": "low", "text": "A1"}, "b": {"means": "mid", "text": "B1"},
                        "c": {"means": "high", "text": "C1"}, "d": {"means": "top", "text": "D1"}},
            "slots": slots,
        })
    }

    async fn vfs_holding(text: &str) -> Arc<MountTable> {
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/", MemoryBackend::new()).await;
        vfs.write_all(Path::new(PATH), text.as_bytes()).await.unwrap();
        vfs
    }

    /// What the scripted council server does.
    struct Behavior {
        capabilities: Value,
        choice_options: u64,
        /// Each option's probability, from its position in the ordering and its id.
        probs: Box<dyn Fn(&[String]) -> Vec<f64> + Send>,
        /// The first decision finds the spec gone: the server answers 404 and forgets it.
        lose_spec_once: bool,
        /// Every decision is a 404.
        decisions_404: bool,
        /// Every decision fails with this status.
        decision_failure: Option<u16>,
        /// A decision whose context has more messages than this is a 413.
        too_large_over: Option<usize>,
    }

    impl Behavior {
        /// A weight per option id, normalized over the options an ordering lists.
        fn weighted(weights: &'static [(&'static str, f64)]) -> Behavior {
            Behavior {
                capabilities: json!(["reads"]),
                choice_options: 26,
                probs: Box::new(move |ids| {
                    let w: Vec<f64> = ids.iter().map(|id| weights.iter().find(|(k, _)| k == id).map_or(1.0, |(_, w)| *w)).collect();
                    let total: f64 = w.iter().sum();
                    w.iter().map(|w| w / total).collect()
                }),
                lose_spec_once: false,
                decisions_404: false,
                decision_failure: None,
                too_large_over: None,
            }
        }
    }

    fn identity(b: &Behavior) -> Reply {
        reply(
            200,
            json!({"models": [{"model": "gemma-4", "aliases": ["qwen"], "status": "ready", "weight_hash": "w4",
                               "tokenizer_hash": "t", "template": "x", "engine": "e",
                               "limits": {"context_tokens": 32768, "decision_context_tokens": 3072, "snapshot_bytes": 1,
                                          "contexts_per_decision": 8, "default_timeout_ms": 2000},
                               "capabilities": b.capabilities}],
                   "limits": {"case_bytes": 1000, "choice_options": b.choice_options},
                   "capabilities": []})
            .to_string(),
        )
    }

    fn decision_response(ids: &[String], p: &[f64]) -> Value {
        let top = (0..p.len()).fold(0, |best, i| if p[i] > p[best] { i } else { best });
        let by_id = |f: &dyn Fn(f64) -> f64| -> Value { ids.iter().zip(p).map(|(id, p)| (id.clone(), json!(f(*p)))).collect::<serde_json::Map<_, _>>().into() };
        json!({
            "model": "gemma-4",
            "answers": {"next": {"type": "choice", "choice": ids[top], "probabilities": by_id(&|p| p), "confidence": p[top], "agree": true, "spread": 0.0}},
            "reads": [{"context": 0, "snapshot": null,
                       "answers": {"next": {"type": "choice", "choice": ids[top], "probabilities": by_id(&|p| p),
                                            "logprobs": by_id(&|p| p.ln()), "mass": 0.0, "confidence": p[top]}},
                       "rendered_hash": format!("blake3:{}", "0".repeat(64))}],
            "pool": {"method": "linear", "weights": "uniform", "normalized": {"next": [1.0]}},
            "identity": {"model": "gemma-4", "weight_hash": "w4", "tokenizer_hash": "t", "template": "x", "engine": "e"},
            "usage": {"prompt": 10, "kept": 4, "fed": 6, "completion": 0, "reasoning": 0},
        })
    }

    /// A council server that holds the specs it is sent and answers each
    /// decision from the options of the spec the decision names.
    async fn council(b: Behavior) -> (String, Arc<Mutex<Vec<Captured>>>) {
        let mut held: HashMap<String, Vec<String>> = HashMap::new();
        let mut decided = 0usize;
        kaijutsu_council::test_server::serve_with(move |c: &Captured| {
            let body = || -> Value { serde_json::from_str(&c.body).unwrap() };
            match (c.method.as_str(), c.path.as_str()) {
                ("GET", "/council/v2/identity") => identity(&b),
                ("POST", "/council/v2/specs") => {
                    let spec = body();
                    let id = gogi_proto::spec_id_of(&spec).unwrap();
                    let options = spec["questions"][0]["criteria"].as_array().unwrap().iter().map(|c| c["option"].as_str().unwrap().to_string());
                    held.insert(id.clone(), options.collect());
                    reply(200, json!({"spec_id": id, "spec": spec}).to_string())
                }
                ("POST", "/council/v2/decisions") => {
                    decided += 1;
                    let id = body()["spec_id"].as_str().unwrap().to_string();
                    if let Some(status) = b.decision_failure {
                        return reply(status, json!({"error": {"type": "internal", "message": "the council is down"}}).to_string());
                    }
                    if b.too_large_over.is_some_and(|most| body()["contexts"][0]["messages"].as_array().unwrap().len() > most) {
                        return reply(413, json!({"error": {"type": "too_large", "message": "context 0 has 3255 tokens, and a decision reads at most 3072"}}).to_string());
                    }
                    let lost = b.lose_spec_once && decided == 1;
                    if lost {
                        held.remove(&id);
                    }
                    let Some(ids) = held.get(&id).filter(|_| !b.decisions_404) else {
                        return reply(404, json!({"error": {"type": "not_found", "message": "no such spec"}}).to_string());
                    };
                    let p = (b.probs)(ids);
                    reply(200, decision_response(ids, &p).to_string())
                }
                other => reply(500, format!("unscripted request {other:?}")),
            }
        })
        .await
    }

    fn opts(vfs: Arc<MountTable>, temperature: Option<f64>) -> BuildOpts {
        let mut o = BuildOpts::new("qwen", 64).with_system("rules").with_vfs(vfs).with_decide(PATH);
        o.temperature = temperature;
        o
    }

    fn turn_text(role: Role, text: &str) -> Message {
        Message { role, content: MessageContent::Blocks(vec![ContentBlock::Text { text: text.into() }]), turn_scoped: false }
    }

    fn history() -> Vec<Message> {
        vec![
            turn_text(Role::User, "u1"),
            turn_text(Role::Assistant, "a1"),
            turn_text(Role::User, "u2"),
            turn_text(Role::Assistant, "a2"),
            turn_text(Role::User, "u3"),
        ]
    }

    fn client(base: &str) -> Client {
        Client::new("nagaya", base, Duration::from_secs(5)).unwrap()
    }

    /// The text a drained stream wrote, and its terminal `Done` or `Error`.
    async fn drain_outcome(mut stream: Stream) -> Result<(String, StreamEvent), (String, StreamError)> {
        let mut text = String::new();
        let mut done = None;
        while let Some(event) = stream.next_event().await {
            match event {
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::Done { .. } => done = Some(event),
                StreamEvent::Error(e) => return Err((text, e)),
                _ => {}
            }
        }
        Ok((text, done.expect("a decision turn ends in done")))
    }

    /// The text a drained stream wrote, and its `Done` event.
    async fn drain(stream: Stream) -> (String, StreamEvent) {
        match drain_outcome(stream).await {
            Ok(res) => res,
            Err((_, e)) => panic!("{e:?}"),
        }
    }

    fn bodies(seen: &Mutex<Vec<Captured>>, path: &str) -> Vec<Value> {
        seen.lock().unwrap().iter().filter(|c| c.path == path).map(|c| serde_json::from_str(&c.body).unwrap()).collect()
    }

    fn options_of(spec: &Value) -> Vec<String> {
        spec["questions"][0]["criteria"].as_array().unwrap().iter().map(|c| c["option"].as_str().unwrap().to_string()).collect()
    }

    /// A turn posts one spec per ordering, sends one decision per ordering per
    /// slot with the trimmed conversation as its one context and the slot's
    /// case after the picks so far, and answers with the picks' text between
    /// the prefix and suffix.
    ///
    /// Falsified by a turn that sends the whole history, drops the system
    /// message, leaves the picks out of the next case, or builds the reply
    /// without the prefix, join, or suffix.
    #[tokio::test]
    async fn a_context_too_large_for_a_decision_drops_its_oldest_messages() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}, {"case": "Bar 2.", "options": ["c", "d"]}]);
        let mut program = program(1, slots);
        program["history_messages"] = json!(4);
        let vfs = vfs_holding(&program.to_string()).await;
        // The system message and 2 more fit; the program keeps 4.
        let (base, seen) = council(Behavior { too_large_over: Some(3), ..Behavior::weighted(&[("b", 9.0), ("d", 9.0)]) }).await;
        let (text, _) = drain(client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap()).await;
        assert_eq!(text, "P:B1|D1]");
        let sizes: Vec<usize> =
            bodies(&seen, "/council/v2/decisions").iter().map(|d| d["contexts"][0]["messages"].as_array().unwrap().len()).collect();
        assert_eq!(sizes, [5, 4, 3, 3], "slot 1 is sent with 4, 3, then 2 kept messages; slot 2 keeps what fit");
    }

    #[tokio::test]
    async fn a_context_too_large_with_one_message_left_fails_the_turn() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}]);
        let vfs = vfs_holding(&program(1, slots).to_string()).await;
        let (base, seen) = council(Behavior { too_large_over: Some(1), ..Behavior::weighted(&[]) }).await;
        let stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        let (text, error) = drain_outcome(stream).await.expect_err("refused");
        assert!(text.is_empty());
        assert_eq!(error.kind, StreamErrorKind::InvalidRequest);
        assert!(error.message.contains("3072"), "{error}");
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 2, "sent with 2 kept messages, then 1, and no more");
    }

    #[tokio::test]
    async fn a_decision_turn_assembles_its_reply_from_decisions() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b", "c"]}, {"case": "Bar 2.", "options": ["b", "c", "d", "a"]}]);
        let vfs = vfs_holding(&program(3, slots).to_string()).await;
        let (base, seen) = council(Behavior::weighted(&[("a", 1.0), ("b", 7.0), ("c", 2.0), ("d", 5.0)])).await;
        let stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        let (text, done) = drain(stream).await;

        assert_eq!(text, "P:B1|D1]", "slot 1 picks b; no_repeat leaves b out, and slot 2 picks d");
        let StreamEvent::Done { stop_reason, input_tokens, output_tokens, extra, .. } = done else { unreachable!() };
        assert_eq!(stop_reason.as_deref(), Some("end_turn"));
        assert_eq!((input_tokens, output_tokens), (Some(60), Some(0)), "total spend across all six decisions");
        let Some(crate::llm::stream::UsageExtra::Council(extra)) = extra else { panic!("{extra:?}") };
        assert_eq!((extra.kept, extra.fed), (24, 36));
        assert!(extra.kept <= input_tokens.unwrap() && extra.fed <= input_tokens.unwrap());
        assert!(extra.seed.is_some() && extra.model == "gemma-4", "{extra:?}");

        let specs = bodies(&seen, "/council/v2/specs");
        assert_eq!(specs.len(), 6, "one spec per ordering per slot: {specs:#?}");
        for spec in &specs {
            assert_eq!(spec["instructions"], "Be the bass.");
            assert_eq!(spec["input_label"], "Phrase");
            assert_eq!(spec["questions"][0]["id"], "next");
            assert_eq!(spec["questions"][0]["type"], "choice");
            assert_eq!(spec["questions"][0]["instructions"], "Next bar?");
        }
        let decisions = bodies(&seen, "/council/v2/decisions");
        assert_eq!(decisions.len(), 6, "one decision per ordering per slot");
        let spec_of = |d: &Value| specs.iter().find(|s| gogi_proto::spec_id_of(s).unwrap() == d["spec_id"].as_str().unwrap()).unwrap().clone();
        for (i, d) in decisions.iter().enumerate() {
            assert_eq!(d["model"], "gemma-4");
            let case = if i < 3 { "Bar 1. So far: (nothing yet)" } else { "Bar 2. So far: B1" };
            assert_eq!(d["case"], case, "decision {i}");
            let contexts = d["contexts"].as_array().unwrap();
            assert_eq!(contexts.len(), 1);
            assert_eq!(contexts[0]["thinking"], false);
            assert!(contexts[0].get("tools").is_none(), "{}", contexts[0]);
            let messages: Vec<(&str, &str)> = contexts[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap()))
                .collect();
            assert_eq!(messages, [("system", "rules"), ("assistant", "a2"), ("user", "u3")], "the system message and the last 2");
        }
        let first_slot: Vec<Vec<String>> = decisions[..3].iter().map(|d| options_of(&spec_of(d))).collect();
        assert_eq!(first_slot[0], ["a", "b", "c"], "ordering 0 is the listed order");
        assert_eq!(spec_of(&decisions[0])["questions"][0]["criteria"][0]["means"], "low: A1");
        for d in &decisions[3..] {
            let mut second = options_of(&spec_of(d));
            second.sort();
            assert_eq!(second, ["a", "c", "d"], "b, the previous pick, is left out");
        }
    }

    /// The pick comes from the orderings pooled, not from one ordering. The
    /// server answers the first-listed option at 0.9 in every ordering, so
    /// ordering 0 favors `a`; with a seed where `b` leads two of the four
    /// orderings, the pooled mean is highest for `b`.
    ///
    /// Falsified by reading only the first decision, or by sampling each
    /// ordering's answer alone.
    #[tokio::test]
    async fn pooling_orderings_beats_one_orderings_bias() {
        let seed = (0u64..)
            .find(|seed| {
                let orders = orderings(&[0, 1, 2], 4, &mut Rng::new(*seed));
                let leads = |o: usize| orders.iter().filter(|x| x[0] == o).count();
                leads(0) == 1 && (leads(1) == 2 || leads(2) == 2)
            })
            .unwrap();
        let orders = orderings(&[0, 1, 2], 4, &mut Rng::new(seed));
        let pooled_winner = if orders.iter().filter(|x| x[0] == 1).count() == 2 { "B1" } else { "C1" };
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b", "c"]}]);
        let text = program(4, slots).to_string();
        let parsed = Program::parse("nagaya", PATH, &text).unwrap();
        let mut b = Behavior::weighted(&[]);
        b.probs = Box::new(|ids| ids.iter().enumerate().map(|(i, _)| if i == 0 { 0.9 } else { 0.05 }).collect());
        let (base, seen) = council(b).await;
        let c = client(&base);
        let stream = c
            .stream_decisions_seeded(parsed, opts(vfs_holding(&text).await, Some(0.0)), history(), c.turn(), seed)
            .await
            .unwrap();
        let (reply_text, _) = drain(stream).await;
        assert_eq!(reply_text, format!("P:{pooled_winner}]"), "the pooled argmax, not ordering 0's first option (A1)");
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 4);
    }

    /// `no_repeat` leaves the previous pick out of the next slot, unless that
    /// would leave fewer than 2 options.
    ///
    /// Falsified by a program that offers the previous pick again, or one that
    /// leaves a slot with a single option.
    #[tokio::test]
    async fn no_repeat_leaves_the_previous_pick_out_unless_two_would_not_remain() {
        let weights: &[(&str, f64)] = &[("a", 1.0), ("b", 7.0), ("c", 2.0)];
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b", "c"]}, {"case": "Bar 2.", "options": ["a", "b", "c"]}]);
        let vfs = vfs_holding(&program(2, slots).to_string()).await;
        let (base, seen) = council(Behavior::weighted(weights)).await;
        let (text, _) = drain(client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap()).await;
        assert_eq!(text, "P:B1|C1]", "b is left out of slot 2, so c wins it");
        let specs = bodies(&seen, "/council/v2/specs");
        let second: Vec<Vec<String>> = bodies(&seen, "/council/v2/decisions")[2..]
            .iter()
            .map(|d| options_of(specs.iter().find(|s| gogi_proto::spec_id_of(s).unwrap() == d["spec_id"].as_str().unwrap()).unwrap()))
            .collect();
        assert!(second.iter().all(|o| !o.contains(&"b".to_string()) && o.len() == 2), "{second:?}");

        let slots = json!([{"case": "Bar 1.", "options": ["a", "b", "c"]}, {"case": "Bar 2.", "options": ["b", "c"]}]);
        let vfs = vfs_holding(&program(1, slots).to_string()).await;
        let (base, seen) = council(Behavior::weighted(weights)).await;
        let (text, _) = drain(client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap()).await;
        assert_eq!(text, "P:B1|B1]", "two options remain only with b, so b stays");
        let last = bodies(&seen, "/council/v2/specs").pop().unwrap();
        assert_eq!(options_of(&last).len(), 2);
    }

    /// A spec the server does not hold on a decision is posted, and the
    /// decision is sent once more. A second `404` fails the turn. A spec
    /// this client has posted is not posted again on a later turn.
    ///
    /// Falsified by a turn that fails on the first 404, retries without
    /// posting, retries without end, or posts the spec on every turn.
    #[tokio::test]
    async fn a_404_for_the_spec_posts_it_and_retries_once() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}]);
        let vfs = vfs_holding(&program(1, slots.clone()).to_string()).await;
        let mut b = Behavior::weighted(&[("a", 1.0), ("b", 3.0)]);
        b.lose_spec_once = true;
        let (base, seen) = council(b).await;
        let c = client(&base);
        let (text, _) = drain(c.stream(opts(vfs.clone(), Some(0.0)), history()).await.unwrap()).await;
        assert_eq!(text, "P:B1]");
        assert_eq!(bodies(&seen, "/council/v2/specs").len(), 2, "posted, forgotten, posted again");
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 2, "the decision is sent once more");

        drain(c.stream(opts(vfs.clone(), Some(0.0)), history()).await.unwrap()).await;
        assert_eq!(bodies(&seen, "/council/v2/specs").len(), 2, "a spec already posted is not posted again");
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 3);

        let mut b = Behavior::weighted(&[]);
        b.decisions_404 = true;
        let (base, seen) = council(b).await;
        let stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        let (_, err) = drain_outcome(stream).await.expect_err("a second 404 fails the turn");
        assert_eq!(err.kind, StreamErrorKind::InvalidRequest);
        assert!(err.message.contains("council backend 'nagaya'"), "{err:?}");
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 2, "one retry, no more");
        assert_eq!(bodies(&seen, "/council/v2/specs").len(), 2);
    }

    /// A decision the server fails fails the turn with the server's error; no
    /// partial reply is returned.
    ///
    /// Falsified by a turn that answers from the slots that did decide.
    #[tokio::test]
    async fn a_failed_decision_fails_the_turn() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}, {"case": "Bar 2.", "options": ["a", "b", "c"]}]);
        let vfs = vfs_holding(&program(1, slots).to_string()).await;
        let mut b = Behavior::weighted(&[]);
        b.decision_failure = Some(500);
        let (base, _) = council(b).await;
        let stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        let (text, err) = drain_outcome(stream).await.expect_err("the turn fails");
        assert!(text.is_empty());
        assert_eq!(err.kind, StreamErrorKind::ServerError);
        assert!(err.message.contains("council backend 'nagaya'") && err.message.contains("the council is down"), "{err:?}");
    }

    /// Every refusal names the program's path and the field, and comes before
    /// any spec or decision is sent.
    ///
    /// Falsified by a turn that falls back to another option, drops a bad
    /// slot, or sends the decisions before it checks the program.
    #[tokio::test]
    async fn a_program_that_cannot_run_is_refused_by_path_and_field() {
        let two = json!([{"case": "Bar 1.", "options": ["a", "b"]}]);
        let cases: Vec<(&str, String, &str)> = vec![
            ("unparseable", "{not json".into(), "is not a decision program"),
            ("no slots", program(1, json!([])).to_string(), "slots: is empty"),
            ("one option", program(1, json!([{"case": "x", "options": ["a"]}])).to_string(), "slots[0].options: lists 1 option"),
            ("unknown option", program(1, json!([{"case": "x", "options": ["a", "zz"]}])).to_string(), "slots[0].options: names option \"zz\""),
            ("repeated option", program(1, json!([{"case": "x", "options": ["a", "a"]}])).to_string(), "names option \"a\" twice"),
            ("shuffle 0", program(0, two.clone()).to_string(), "shuffle: is 0"),
            ("shuffle 9", program(9, two.clone()).to_string(), "shuffle: is 9"),
            ("history 0", program(1, two.clone()).to_string().replace("\"history_messages\":2", "\"history_messages\":0"), "history_messages: is 0"),
            ("stray member", program(1, two.clone()).to_string().replace("\"shuffle\"", "\"shufle\""), "shufle"),
        ];
        for (what, text, expect) in cases {
            let vfs = vfs_holding(&text).await;
            let (base, seen) = council(Behavior::weighted(&[])).await;
            let err = client(&base).stream(opts(vfs, None), history()).await.err().unwrap_or_else(|| panic!("{what}: accepted"));
            let LlmError::InvalidRequest(m) = &err else { panic!("{what}: {err:?}") };
            assert!(m.contains(PATH) && m.contains(expect), "{what}: {m}");
            assert!(seen.lock().unwrap().is_empty(), "{what}: nothing is sent for a program that cannot run");
        }

        let vfs = vfs_holding("{}").await;
        let (base, _) = council(Behavior::weighted(&[])).await;
        let mut o = opts(vfs, None);
        o.decide = Some("/missing.json".into());
        let err = client(&base).stream(o, history()).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("/missing.json") && m.contains("cannot be read")), "{err:?}");

        let (base, _) = council(Behavior::weighted(&[])).await;
        let mut o = opts(Arc::new(MountTable::new()), None);
        o.vfs = None;
        let err = client(&base).stream(o, history()).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains(PATH) && m.contains("no VFS")), "{err:?}");
    }

    /// The served model must list `reads`, and a slot may list no more options
    /// than the server's `limits.choice_options`. Only the identity is read
    /// before either refusal.
    ///
    /// Falsified by a turn that sends decisions to a model that cannot read
    /// them, or that leaves the option limit to the server's 400.
    #[tokio::test]
    async fn the_server_must_read_decisions_and_take_the_options() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b", "c"]}]);
        let vfs = vfs_holding(&program(1, slots).to_string()).await;
        let mut b = Behavior::weighted(&[]);
        b.capabilities = json!(["generate"]);
        let (base, seen) = council(b).await;
        let err = client(&base).stream(opts(vfs.clone(), None), history()).await.err().unwrap();
        assert!(
            matches!(&err, LlmError::InvalidRequest(m) if m.contains(PATH) && m.contains("gemma-4") && m.contains("does not list reads")),
            "{err:?}"
        );
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/council/v2/identity"]);

        let mut b = Behavior::weighted(&[]);
        b.choice_options = 2;
        let (base, seen) = council(b).await;
        let err = client(&base).stream(opts(vfs, None), history()).await.err().unwrap();
        assert!(
            matches!(&err, LlmError::InvalidRequest(m) if m.contains(PATH) && m.contains("slots[0].options") && m.contains("limits.choice_options")),
            "{err:?}"
        );
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/council/v2/identity"]);
    }

    /// `T == 0` is the argmax with ties to the earlier option; a positive `T`
    /// weights `p^(1/T)`, and the same seed gives the same sample.
    #[test]
    fn sampling_follows_the_temperature() {
        let mut rng = Rng::new(1);
        assert_eq!(sample(&[0.4, 0.4, 0.2], Some(0.0), &mut rng).unwrap(), 0, "ties go to the earlier option");
        assert_eq!(sample(&[0.1, 0.6, 0.3], Some(0.0), &mut rng).unwrap(), 1);
        let picks = |t: Option<f64>| (0..400).map(|i| sample(&[0.1, 0.6, 0.3], t, &mut Rng::new(i)).unwrap()).collect::<Vec<_>>();
        let share = |picks: &[usize], i: usize| picks.iter().filter(|p| **p == i).count() as f64 / picks.len() as f64;
        assert!(share(&picks(Some(0.1)), 1) > 0.99, "a low temperature sharpens toward the top option");
        let flat = picks(Some(2.0));
        assert!(share(&flat, 1) < 0.6 && share(&flat, 0) > 0.1, "a high temperature flattens");
        assert_eq!(picks(None), picks(Some(1.0)), "an absent temperature is 1");
        assert!(sample(&[0.5, 0.5], Some(-1.0), &mut rng).is_err());
        assert!(sample(&[0.0, 0.0], Some(1.0), &mut rng).is_err());
    }

    /// Ordering 0 is the listed order; the others are permutations, the same
    /// for the same seed and different from each other when that is possible.
    #[test]
    fn orderings_start_with_the_listed_order_and_replay_from_the_seed() {
        let options = [3, 1, 4, 5];
        let orders = orderings(&options, 4, &mut Rng::new(9));
        assert_eq!(orders[0], options);
        assert_eq!(orders, orderings(&options, 4, &mut Rng::new(9)));
        for order in &orders {
            let mut sorted = order.clone();
            sorted.sort();
            assert_eq!(sorted, [1, 3, 4, 5]);
        }
        let distinct: HashSet<&Vec<usize>> = orders.iter().collect();
        assert_eq!(distinct.len(), 4);
    }

    #[test]
    fn orderings_cap_at_factorial_and_enumerate_all_permutations_without_duplicates() {
        for (options, count, expected_len) in [
            (&[0, 1][..], 4, 2),
            (&[0, 1, 2][..], 6, 6),
            (&[0, 1, 2][..], 8, 6),
            (&[0, 1, 2][..], 5, 5),
            (&[7][..], 4, 1),
            (&[1, 2, 3, 4][..], 8, 8),
        ] {
            let orders = orderings(options, count, &mut Rng::new(42));
            assert_eq!(orders.len(), expected_len, "options={options:?} count={count}");
            assert_eq!(orders[0], options, "ordering 0 is always the listed order");
            let distinct: HashSet<&Vec<usize>> = orders.iter().collect();
            assert_eq!(distinct.len(), expected_len, "no duplicates for options={options:?} count={count}: {orders:?}");
            for o in &orders {
                let mut a = o.clone();
                let mut b = options.to_vec();
                a.sort();
                b.sort();
                assert_eq!(a, b);
            }
        }
    }

    #[tokio::test]
    async fn a_decision_turn_dumps_every_decision_without_collision() {
        use crate::llm::request_dump::{RequestTrace, wait_for_files};
        use kaijutsu_types::{ContextId, TurnId};
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b", "c"]}, {"case": "Bar 2.", "options": ["b", "c", "d", "a"]}]);
        let vfs = vfs_holding(&program(3, slots).to_string()).await;
        let (base, seen) = council(Behavior::weighted(&[("a", 1.0), ("b", 7.0), ("c", 2.0), ("d", 5.0)])).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts(vfs, Some(0.0));
        o.trace = Some(Arc::new(RequestTrace::new(ContextId::new(), TurnId::new(), 2, Some(tmp.path().to_path_buf()))));
        drain(client(&base).stream(o, history()).await.unwrap()).await;
        let files = wait_for_files(tmp.path(), 6).await;
        assert_eq!(files.len(), 6);
        let dumped_cases: Vec<Value> = files
            .iter()
            .map(|p| {
                let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
                v["body"]["case"].clone()
            })
            .collect();
        assert_eq!(dumped_cases.iter().filter(|c| c.as_str().unwrap().starts_with("Bar 1.")).count(), 3);
        assert_eq!(dumped_cases.iter().filter(|c| c.as_str().unwrap().starts_with("Bar 2.")).count(), 3);
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 6);
    }

    #[tokio::test]
    async fn a_later_slot_failure_preserves_spent_usage_on_error() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}, {"case": "Bar 2.", "options": ["a", "b"]}]);
        let vfs = vfs_holding(&program(2, slots).to_string()).await;
        let mut held: HashMap<String, Vec<String>> = HashMap::new();
        let mut decided = 0usize;
        let b = Behavior::weighted(&[("a", 1.0), ("b", 3.0)]);
        let (base, _) = kaijutsu_council::test_server::serve_with(move |c: &Captured| {
            let body = || -> Value { serde_json::from_str(&c.body).unwrap() };
            match (c.method.as_str(), c.path.as_str()) {
                ("GET", "/council/v2/identity") => identity(&b),
                ("POST", "/council/v2/specs") => {
                    let spec = body();
                    let id = gogi_proto::spec_id_of(&spec).unwrap();
                    let options = spec["questions"][0]["criteria"].as_array().unwrap().iter().map(|c| c["option"].as_str().unwrap().to_string());
                    held.insert(id.clone(), options.collect());
                    reply(200, json!({"spec_id": id, "spec": spec}).to_string())
                }
                ("POST", "/council/v2/decisions") => {
                    decided += 1;
                    if decided > 2 {
                        return reply(500, json!({"error": {"type": "internal", "message": "slot 2 failed"}}).to_string());
                    }
                    let id = body()["spec_id"].as_str().unwrap().to_string();
                    let ids = held.get(&id).unwrap();
                    let p = (b.probs)(ids);
                    reply(200, decision_response(ids, &p).to_string())
                }
                other => reply(500, format!("unscripted request {other:?}")),
            }
        })
        .await;
        let stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        let (text, err) = drain_outcome(stream).await.expect_err("slot 2 fails");
        assert!(text.is_empty());
        assert_eq!(err.kind, StreamErrorKind::ServerError);
        let spent = err.usage.expect("completed slot 1 spend must be preserved on error");
        assert_eq!(spent.input_tokens, Some(20));
        assert_eq!(spent.output_tokens, Some(0));
        let Some(UsageExtra::Council(extra)) = spent.extra else { panic!("{spent:?}") };
        assert_eq!((extra.kept, extra.fed), (8, 12));
    }

    #[tokio::test]
    async fn a_slot_with_two_options_and_shuffle_4_sends_only_two_decisions() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}]);
        let vfs = vfs_holding(&program(4, slots).to_string()).await;
        let (base, seen) = council(Behavior::weighted(&[("a", 1.0), ("b", 3.0)])).await;
        let (text, done) = drain(client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap()).await;
        assert_eq!(text, "P:B1]");
        assert_eq!(bodies(&seen, "/council/v2/decisions").len(), 2, "2! = 2 orderings, not 4");
        assert!(matches!(done, StreamEvent::Done { input_tokens: Some(20), .. }));
    }

    #[tokio::test]
    async fn cancelling_a_decision_stream_mid_program_emits_cancel_done_with_spent_usage() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}, {"case": "Bar 2.", "options": ["a", "b"]}]);
        let vfs = vfs_holding(&program(2, slots).to_string()).await;
        let mut held: HashMap<String, Vec<String>> = HashMap::new();
        let decided = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let slot2_started = Arc::new(tokio::sync::Notify::new());
        let decided_srv = decided.clone();
        let slot2_srv = slot2_started.clone();
        let b = Behavior::weighted(&[("a", 1.0), ("b", 3.0)]);
        let (base, _) = kaijutsu_council::test_server::serve_with(move |c: &Captured| {
            let body = || -> Value { serde_json::from_str(&c.body).unwrap() };
            match (c.method.as_str(), c.path.as_str()) {
                ("GET", "/council/v2/identity") => identity(&b),
                ("POST", "/council/v2/specs") => {
                    let spec = body();
                    let id = gogi_proto::spec_id_of(&spec).unwrap();
                    let options = spec["questions"][0]["criteria"].as_array().unwrap().iter().map(|c| c["option"].as_str().unwrap().to_string());
                    held.insert(id.clone(), options.collect());
                    reply(200, json!({"spec_id": id, "spec": spec}).to_string())
                }
                ("POST", "/council/v2/decisions") => {
                    let n = decided_srv.fetch_add(1, Ordering::SeqCst) + 1;
                    if n > 2 {
                        if n == 3 {
                            slot2_srv.notify_one();
                        }
                        let mut r = reply(200, decision_response(&["a".into(), "b".into()], &[0.5, 0.5]).to_string());
                        r.delay = Duration::from_secs(5);
                        return r;
                    }
                    let id = body()["spec_id"].as_str().unwrap().to_string();
                    let ids = held.get(&id).unwrap();
                    let p = (b.probs)(ids);
                    reply(200, decision_response(ids, &p).to_string())
                }
                other => reply(500, format!("unscripted request {other:?}")),
            }
        })
        .await;
        let mut stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        tokio::select! {
            _ = slot2_started.notified() => {}
            ev = stream.next_event() => panic!("unexpectedly finished before slot 2 stalled: {ev:?}"),
        }
        stream.cancel();
        let (text, done) = drain(stream).await;
        assert!(text.is_empty());
        let StreamEvent::Done { stop_reason, input_tokens, output_tokens, extra } = done else { unreachable!() };
        assert_eq!(stop_reason, None);
        assert_eq!((input_tokens, output_tokens), (Some(20), Some(0)));
        let Some(UsageExtra::Council(extra)) = extra else { panic!("{extra:?}") };
        assert_eq!((extra.kept, extra.fed), (8, 12));
    }

    #[tokio::test]
    async fn cancelling_before_any_decision_completes_emits_empty_cancel_done() {
        let slots = json!([{"case": "Bar 1.", "options": ["a", "b"]}]);
        let vfs = vfs_holding(&program(1, slots).to_string()).await;
        let b = Behavior::weighted(&[("a", 1.0), ("b", 3.0)]);
        let (base, _) = kaijutsu_council::test_server::serve_with(move |c: &Captured| {
            let body = || -> Value { serde_json::from_str(&c.body).unwrap() };
            match (c.method.as_str(), c.path.as_str()) {
                ("GET", "/council/v2/identity") => identity(&b),
                ("POST", "/council/v2/specs") => {
                    let spec = body();
                    let id = gogi_proto::spec_id_of(&spec).unwrap();
                    reply(200, json!({"spec_id": id, "spec": spec}).to_string())
                }
                ("POST", "/council/v2/decisions") => {
                    let mut r = reply(200, decision_response(&["a".into(), "b".into()], &[0.5, 0.5]).to_string());
                    r.delay = Duration::from_secs(5);
                    r
                }
                other => reply(500, format!("unscripted request {other:?}")),
            }
        })
        .await;
        let stream = client(&base).stream(opts(vfs, Some(0.0)), history()).await.unwrap();
        stream.cancel();
        let (text, done) = drain(stream).await;
        assert!(text.is_empty());
        assert_eq!(
            done,
            StreamEvent::Done { stop_reason: None, input_tokens: None, output_tokens: None, extra: None }
        );
    }

    /// The length in eighths of each note or rest of a bar of ABC at `L:1/8`:
    /// a note is one unit and a number after it is its length.
    fn note_lengths(bar: &str) -> Vec<u32> {
        let mut lengths: Vec<u32> = Vec::new();
        for c in bar.chars() {
            match c {
                'A'..='G' | 'z' => lengths.push(1),
                '0'..='9' => *lengths.last_mut().unwrap() = c.to_digit(10).unwrap(),
                ',' | '\'' | ' ' => {}
                other => panic!("unexpected {other:?} in {bar:?}"),
            }
        }
        lengths
    }

    fn eighths(bar: &str) -> u32 {
        note_lengths(bar).iter().sum()
    }

    /// The program the musician seeds parses, every slot chooses among at
    /// least 2 options, every bar fills 4/4, and the rest endings are only
    /// in the last slot (with bar 4 allowing the midpoint turnaround).
    ///
    /// Falsified by a bar that is short or long, a slot with one option, or
    /// a rest ending offered before the last bar.
    #[test]
    fn the_shipped_bass_program_parses_and_its_bars_fill_the_measure() {
        let path = "/config/rc/musician/programs/bass-bars.json";
        let text = crate::seed_scripts::seed_body(path).expect("the program ships as an rc seed");
        let program = Program::parse("nagaya", path, text).unwrap();
        assert_eq!((program.options.len(), program.slots.len(), program.shuffle, program.no_repeat), (12, 8, 2, true));
        assert!(program.slots.iter().all(|s| s.options.len() >= 2));
        for option in &program.options {
            assert_eq!(eighths(&option.text), 8, "{}: {}", option.id, option.text);
        }
        let ending = |o: usize| note_lengths(&program.options[o].text).iter().any(|n| *n >= 4);
        let rest_ending = |o: usize| ending(o) && program.options[o].text.contains('z');
        let (last, rest) = program.slots.split_last().unwrap();
        assert!(last.options.iter().all(|o| ending(*o)), "the last bar holds a note");
        assert!(rest.iter().all(|s| s.options.iter().all(|o| !rest_ending(*o))), "no rest ending before the last bar");
        for (i, slot) in rest.iter().enumerate() {
            if i != 3 {
                assert!(slot.options.iter().all(|o| !ending(*o)), "no held note in bar {}", i + 1);
            }
        }
        let reply = program.reply_text(&[0, 1, 2, 3, 0, 1, 2, 4]);
        assert!(
            reply.starts_with("X:1\n%%MIDI channel 1\n%%MIDI program 33\nM:4/4\nL:1/8\nK:Em clef=bass\n")
                && reply.ends_with(" |]")
                && reply.contains(" | "),
            "{reply}"
        );
    }
}
