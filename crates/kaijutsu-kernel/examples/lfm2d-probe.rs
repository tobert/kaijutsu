//! Score kaijutsu's own clause families against the live lfm2d scorer.
//!
//! Rust port of `contrib/lfm2d-probe.py`. Every behavior that script argues
//! for in its comments applies here too, plus one deliberate divergence:
//! this program never degrades a failure into a table cell. A scorer that
//! is unreachable, a malformed response, or a missing field prints what
//! went wrong and exits non-zero -- it does not print `NaN` and carry on.
//!
//!     cargo run --example lfm2d-probe -p kaijutsu-kernel -- --help
//!
//! Verify the checkpoint before trusting a comparison: label ORDER carries
//! the ordinal mapping, and a swap that reorders labels breaks it silently.
//! `--solo` exists to rule out cascade aggregation artifacts -- if batched
//! and solo scores disagree for a clause, the batched number is not
//! citable.
//!
//! Expectations in the corpus are kaijutsu's read on each clause, not the
//! scorer's. Results and the standing arguments: docs/issues.md, "lfm2d
//! risk scoring".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use kaijutsu_kernel::kj::corpus::{self, AliasPair, Corpus, Severity};
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(
    about = "Score kaijutsu's own clause families against the live lfm2d scorer.",
    long_about = "Sends clause TEXT to a classifier over HTTP and prints the scores. It \
runs none of the clauses locally, and it never writes to the kernel.\n\n\
Verify the checkpoint before trusting a comparison: label ORDER carries the \
ordinal mapping, and a swap that reorders labels breaks it silently. --solo \
exists to rule out cascade aggregation artifacts -- if batched and solo \
scores disagree for a clause, the batched number is not citable.\n\n\
Expectations in the corpus are kaijutsu's read on each clause, not the \
scorer's. Results and the standing arguments: docs/issues.md, \"lfm2d risk \
scoring\"."
)]
struct Cli {
    /// Scorer base URL.
    #[arg(long, default_value = "http://lfm2d-1.taila4abc.ts.net:8088")]
    url: String,

    /// One request per clause instead of one batched call, to rule out
    /// cascade aggregation artifacts. If batched and solo scores disagree
    /// for a clause, the batched number is not citable.
    #[arg(long, conflicts_with_all = ["dump_corpus", "measured", "aliases"])]
    solo: bool,

    /// Apply an auto-allow band: data-critical score below DC auto-allows.
    /// DC must be read off THIS live head's response below -- it is not a
    /// constant. Never pin a floor value across checkpoints; a new head can
    /// move the whole distribution out from under it.
    #[arg(long, value_name = "DC", conflicts_with_all = ["dump_corpus", "measured", "aliases"])]
    floor: Option<f64>,

    /// Per-request timeout, in seconds.
    #[arg(long, default_value_t = 60.0)]
    timeout: f64,

    /// Dump the raw batched /v1/cascade response here. Ignored under
    /// --solo (solo makes one request per clause, so there is no single
    /// raw response to dump).
    #[arg(long, value_name = "PATH", conflicts_with_all = ["dump_corpus", "measured", "aliases"])]
    json: Option<PathBuf>,

    /// Score both spellings of every alias pair and report argmax splits.
    /// Shared surface: the lfm2d lane uses this output as the acceptance
    /// test for their kj-verb training slice -- keep the columns and the
    /// summary line stable.
    #[arg(long, conflicts_with_all = ["dump_corpus", "measured", "solo", "floor", "json"])]
    aliases: bool,

    /// Write the whole corpus as JSON and exit without contacting the
    /// scorer, so the lfm2d lane never needs a Rust toolchain. Bare flag
    /// writes contrib/kj-corpus.json; pass a path to write elsewhere.
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        default_missing_value = "contrib/kj-corpus.json",
        conflicts_with_all = ["measured", "aliases", "solo", "floor", "json"]
    )]
    dump_corpus: Option<PathBuf>,

    /// Aggregate a `kj ledger list --signals --history --json` dump
    /// instead of contacting the scorer. Counts ONLY seq == 0 (the
    /// decision signal) -- every non-winning clause in a call is stamped
    /// `escalate` unconditionally by the rc hook, and aggregating every
    /// signal reads far above the true ask-level rate. See docs/issues.md,
    /// "The ledger cannot be counted".
    #[arg(long, value_name = "PATH", conflicts_with_all = ["dump_corpus", "aliases", "solo", "floor", "json"])]
    measured: Option<PathBuf>,
}

// --- /v1/models -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ModelInfo {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    weight_hash: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
}

struct Head {
    id: String,
    weight_hash: String,
    labels: Vec<String>,
}

async fn fetch_models(client: &reqwest::Client, url: &str) -> Result<Vec<ModelInfo>> {
    let endpoint = format!("{url}/v1/models");
    let resp = client
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("GET {endpoint} failed -- scorer unreachable"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("GET {endpoint} returned a body that could not be read"))?;
    if !status.is_success() {
        bail!("GET {endpoint} returned {status}: {}", truncate_for_error(&body));
    }
    serde_json::from_str(&body).with_context(|| {
        format!(
            "GET {endpoint} returned a response that does not match the expected shape (array of model objects): {}",
            truncate_for_error(&body)
        )
    })
}

/// Picks the classifier head and requires every field this program prints
/// or compares against. A response missing one of these is a malformed
/// response -- fail loudly rather than print `checkpoint: None`.
fn select_head(models: &[ModelInfo]) -> Result<Head> {
    let classifier = models
        .iter()
        .find(|m| m.kind.as_deref() == Some("classifier"))
        .context("GET /v1/models returned no entry with kind == \"classifier\"")?;
    let id = classifier
        .id
        .clone()
        .context("the classifier entry from /v1/models is missing \"id\"")?;
    let weight_hash = classifier
        .weight_hash
        .clone()
        .context("the classifier entry from /v1/models is missing \"weight_hash\"")?;
    let labels = classifier
        .labels
        .clone()
        .context("the classifier entry from /v1/models is missing \"labels\"")?;
    Ok(Head { id, weight_hash, labels })
}

fn print_head(head: &Head) {
    let hash_prefix: String = head.weight_hash.chars().take(12).collect();
    println!("checkpoint: {}  weight_hash {}", head.id, hash_prefix);
    println!("labels:     {:?}", head.labels);
    println!("            ^ ORDER carries the ordinal mapping -- a reorder breaks it silently");
    println!();
}

// --- /v1/cascade --------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
struct ClauseScore {
    index: usize,
    top_severity: String,
    severity_scores: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct CascadeResponse {
    clauses: Vec<ClauseScore>,
}

/// Truncated so a huge malformed body doesn't flood the terminal -- the
/// point is a diagnosable error message, not a full dump (`--json` exists
/// for that).
fn truncate_for_error(s: &str) -> String {
    const LIMIT: usize = 2000;
    if s.len() <= LIMIT {
        s.to_string()
    } else {
        format!("{}... ({} bytes total)", &s[..LIMIT], s.len())
    }
}

async fn post_cascade_raw(
    client: &reqwest::Client,
    cascade_url: &str,
    clauses: &[String],
) -> Result<serde_json::Value> {
    let resp = client
        .post(cascade_url)
        .json(&serde_json::json!({ "clauses": clauses }))
        .send()
        .await
        .with_context(|| format!("POST {cascade_url} failed -- scorer unreachable"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("POST {cascade_url} returned a body that could not be read"))?;
    if !status.is_success() {
        bail!("POST {cascade_url} returned {status}: {}", truncate_for_error(&body));
    }
    serde_json::from_str(&body).with_context(|| {
        format!(
            "POST {cascade_url} returned a body that is not valid JSON: {}",
            truncate_for_error(&body)
        )
    })
}

fn parse_cascade(cascade_url: &str, raw: &serde_json::Value) -> Result<CascadeResponse> {
    serde_json::from_value(raw.clone()).with_context(|| {
        format!(
            "POST {cascade_url} returned a response that does not match the expected cascade shape: {}",
            truncate_for_error(&raw.to_string())
        )
    })
}

fn extract_scores(label: &str, clause: &str, c: &ClauseScore) -> Result<(f64, f64, f64)> {
    let get = |key: &str| -> Result<f64> {
        c.severity_scores.get(key).copied().with_context(|| {
            format!("cascade response for [{label}] {clause:?} is missing severity_scores[\"{key}\"]")
        })
    };
    Ok((get("data-critical")?, get("situation-normal")?, get("informative")?))
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Informative => "informative",
        Severity::SituationNormal => "situation-normal",
        Severity::DataCritical => "data-critical",
    }
}

// --- corpus flattening + scoring ----------------------------------------

/// One corpus clause, flattened to the shape the scorer table wants. The
/// `label` column is `family/path`: a verb's `path` ("context remove") or
/// an extra's `family` ("severity") -- whichever the source row carries.
struct Probe {
    label: String,
    clause: String,
    expect: Severity,
}

fn flatten_corpus(c: &Corpus) -> Vec<Probe> {
    let mut probes = Vec::with_capacity(c.verbs.len() + c.extras.len());
    for v in &c.verbs {
        probes.push(Probe {
            label: v.path.clone(),
            clause: v.clause.clone(),
            expect: v.expect,
        });
    }
    for e in &c.extras {
        probes.push(Probe {
            label: e.family.clone(),
            clause: e.clause.clone(),
            expect: e.expect,
        });
    }
    probes
}

struct ScoredRow {
    label: String,
    clause: String,
    expect: Severity,
    top: String,
    dc: f64,
    sn: f64,
    inf: f64,
}

async fn score_batched(
    client: &reqwest::Client,
    cascade_url: &str,
    probes: &[Probe],
) -> Result<(Vec<ScoredRow>, serde_json::Value)> {
    let clauses: Vec<String> = probes.iter().map(|p| p.clause.clone()).collect();
    let raw = post_cascade_raw(client, cascade_url, &clauses).await?;
    let parsed = parse_cascade(cascade_url, &raw)?;
    let mut by_index: BTreeMap<usize, ClauseScore> = BTreeMap::new();
    for c in parsed.clauses {
        by_index.insert(c.index, c);
    }
    let mut rows = Vec::with_capacity(probes.len());
    for (i, p) in probes.iter().enumerate() {
        let c = by_index
            .get(&i)
            .with_context(|| format!("cascade response is missing clause index {i} for [{}] {:?}", p.label, p.clause))?;
        let (dc, sn, inf) = extract_scores(&p.label, &p.clause, c)?;
        rows.push(ScoredRow {
            label: p.label.clone(),
            clause: p.clause.clone(),
            expect: p.expect,
            top: c.top_severity.clone(),
            dc,
            sn,
            inf,
        });
    }
    Ok((rows, raw))
}

async fn score_solo(client: &reqwest::Client, cascade_url: &str, probes: &[Probe]) -> Result<Vec<ScoredRow>> {
    let mut rows = Vec::with_capacity(probes.len());
    for p in probes {
        let raw = post_cascade_raw(client, cascade_url, std::slice::from_ref(&p.clause)).await?;
        let parsed = parse_cascade(cascade_url, &raw)?;
        let c = parsed
            .clauses
            .first()
            .with_context(|| format!("solo POST for [{}] {:?} returned zero clauses", p.label, p.clause))?;
        let (dc, sn, inf) = extract_scores(&p.label, &p.clause, c)?;
        rows.push(ScoredRow {
            label: p.label.clone(),
            clause: p.clause.clone(),
            expect: p.expect,
            top: c.top_severity.clone(),
            dc,
            sn,
            inf,
        });
    }
    Ok(rows)
}

fn print_scored_table(rows: &[ScoredRow]) {
    let hdr = format!(
        "{:<28} {:<17} {:>6} {:>6} {:>6}  {:<22} clause",
        "family/path", "top", "dc", "sn", "inf", "expected"
    );
    println!("{hdr}");
    println!("{}", "-".repeat(hdr.len()));
    for r in rows {
        println!(
            "{:<28} {:<17} {:>6.3} {:>6.3} {:>6.3}  {:<22} {}",
            r.label,
            r.top,
            r.dc,
            r.sn,
            r.inf,
            severity_str(r.expect),
            r.clause
        );
    }
}

/// In `escalate` mode any verdict that is not the benign label asks a
/// human. This set is hand-picked and destructive-heavy -- it is NOT a
/// traffic sample, and this rate is not comparable to a measured live
/// window (see `--measured`).
fn print_escalate_summary(rows: &[ScoredRow]) {
    let asks = rows.iter().filter(|r| r.top != "informative").count();
    println!();
    println!(
        "escalate-mode asks: {asks}/{} = {:.1}% of THIS set",
        rows.len(),
        100.0 * asks as f64 / rows.len() as f64
    );
    println!("  This set is hand-picked and destructive-heavy. It is NOT a traffic");
    println!("  sample -- do not compare this rate to a measured live window.");
}

fn print_floor(rows: &[ScoredRow], floor: f64) {
    println!();
    println!("--- auto-allow band: data-critical < {floor} ---");
    let mut misses = 0u32;
    for r in rows {
        if r.dc >= floor {
            continue;
        }
        let flag = if r.expect == Severity::DataCritical {
            misses += 1;
            "   <== MISS: we hold this data-critical"
        } else {
            ""
        };
        println!("AUTO-ALLOW  dc={:5.3}  {}{}", r.dc, r.clause, flag);
    }
    println!();
    println!("{misses} shape(s) we hold data-critical would auto-allow at this floor.");
}

// --- --aliases ------------------------------------------------------------

async fn run_aliases(client: &reqwest::Client, cascade_url: &str, pairs: &[AliasPair]) -> Result<()> {
    println!(
        "{:<20} {:<40} {:<17} {:>6}   {:<40} {:<17} {:>6}  split?",
        "op", "canonical", "top", "dc", "alias", "top", "dc"
    );
    println!("{}", "-".repeat(150));
    let mut splits = 0usize;
    for pair in pairs {
        let c_raw = post_cascade_raw(client, cascade_url, std::slice::from_ref(&pair.canonical)).await?;
        let c_parsed = parse_cascade(cascade_url, &c_raw)?;
        let c_score = c_parsed
            .clauses
            .first()
            .with_context(|| format!("alias POST for canonical [{}] {:?} returned zero clauses", pair.path, pair.canonical))?;
        let (c_dc, _, _) = extract_scores(&pair.path, &pair.canonical, c_score)?;

        let a_raw = post_cascade_raw(client, cascade_url, std::slice::from_ref(&pair.alias)).await?;
        let a_parsed = parse_cascade(cascade_url, &a_raw)?;
        let a_score = a_parsed
            .clauses
            .first()
            .with_context(|| format!("alias POST for alias [{}] {:?} returned zero clauses", pair.path, pair.alias))?;
        let (a_dc, _, _) = extract_scores(&pair.path, &pair.alias, a_score)?;

        let split = c_score.top_severity != a_score.top_severity;
        if split {
            splits += 1;
        }
        println!(
            "{:<20} {:<40} {:<17} {:>6.3}   {:<40} {:<17} {:>6.3}  {}",
            pair.path,
            pair.canonical,
            c_score.top_severity,
            c_dc,
            pair.alias,
            a_score.top_severity,
            a_dc,
            if split { "SPLIT" } else { "" }
        );
    }
    println!();
    println!("{splits}/{} live alias pairs disagree on argmax severity.", pairs.len());
    println!("  Same handler, different verdict. Under a data-critical floor the");
    println!("  cheaper spelling auto-allows an operation the other escalates.");
    Ok(())
}

// --- --dump-corpus ----------------------------------------------------

fn run_dump_corpus(path: &Path, corpus: &Corpus) -> Result<()> {
    let json = serde_json::to_string_pretty(corpus).context("serializing the corpus to JSON")?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    std::fs::write(path, json).with_context(|| format!("writing {path:?}"))?;
    println!(
        "wrote corpus ({} verbs, {} extras) to {path:?}",
        corpus.verbs.len(),
        corpus.extras.len()
    );
    Ok(())
}

// --- --measured ---------------------------------------------------------

/// Mirrors one signal row from `signal_row_json` in
/// `crates/kaijutsu-kernel/src/kj/ledger.rs` -- only the fields this mode
/// actually reads.
#[derive(Debug, Deserialize)]
struct MeasuredSignal {
    seq: i64,
    #[serde(default)]
    model_id: Option<String>,
    verdict: String,
    /// Absent on the fallback path. The rc hook records a clause's position
    /// only when it came from `KJ_TOOL_PLAN`; when kaish cannot plan the
    /// command, the hook scores the whole raw command as one clause and has
    /// no position to record. A null here is therefore the marker for
    /// "this ask was scored without per-command granularity".
    #[serde(default)]
    stmt_seq: Option<i64>,
    #[serde(default)]
    cmd_seq: Option<i64>,
}

/// Mirrors one `{request_id, signals}` element of `kj ledger list
/// --signals --history --json`'s `.data`.
#[derive(Debug, Deserialize)]
struct MeasuredRequest {
    #[allow(dead_code)]
    request_id: String,
    signals: Vec<MeasuredSignal>,
}

#[derive(Default)]
struct ModelAgg {
    asks: u64,
    escalations: u64,
    /// Asks scored from the whole raw command because kaish could not plan
    /// it. Worth watching: the lfm2d lane measured this path firing 2.6% of
    /// clauses against 0.16% on the plan path, so a rise here is a rise in
    /// noise, and its cause is a kaish parse failure rather than anything
    /// the classifier did.
    fallback: u64,
}

/// A checkpoint below this many primary asks is not a measurement -- our
/// own v10 sample was 6 and must not be quoted as a rate.
const MIN_ASKS_FOR_A_RATE: u64 = 30;

fn run_measured(path: &Path) -> Result<()> {
    let body = std::fs::read_to_string(path).with_context(|| format!("could not read {path:?}"))?;
    let requests: Vec<MeasuredRequest> = serde_json::from_str(&body).with_context(|| {
        format!(
            "{path:?} does not match the `kj ledger list --signals --history --json` shape \
             (an array of {{request_id, signals}} objects)"
        )
    })?;

    // Only seq == 0 carries a decision. Every non-winning clause in a call
    // is stamped `escalate` unconditionally by the rc hook
    // (assets/defaults/rc/lib/create/S50-lfm2d.kai), so counting every
    // signal reads 21.3% where the true ask-level rate is 4.2%. See
    // docs/issues.md, "The ledger cannot be counted".
    let mut by_model: BTreeMap<String, ModelAgg> = BTreeMap::new();
    let mut secondary_skipped = 0u64;
    let mut no_primary = 0u64;

    for req in &requests {
        let mut saw_primary = false;
        for sig in &req.signals {
            if sig.seq != 0 {
                secondary_skipped += 1;
                continue;
            }
            saw_primary = true;
            let key = sig.model_id.clone().unwrap_or_else(|| "(unknown model_id)".to_string());
            let agg = by_model.entry(key).or_default();
            agg.asks += 1;
            if sig.verdict == "escalate" {
                agg.escalations += 1;
            }
            if sig.stmt_seq.is_none() || sig.cmd_seq.is_none() {
                agg.fallback += 1;
            }
        }
        if !saw_primary {
            no_primary += 1;
        }
    }

    if by_model.is_empty() {
        bail!(
            "{path:?} carried {} request(s) but zero seq == 0 signals -- nothing to measure",
            requests.len()
        );
    }

    println!(
        "{:<28} {:>8} {:>12} {:>8} {:>10}",
        "model_id", "asks", "escalations", "rate", "no-plan"
    );
    println!("{}", "-".repeat(71));
    for (model, agg) in &by_model {
        let rate = 100.0 * agg.escalations as f64 / agg.asks as f64;
        println!(
            "{model:<28} {:>8} {:>12} {rate:>7.1}% {:>10}",
            agg.asks, agg.escalations, agg.fallback
        );
    }
    println!();
    println!(
        "skipped {secondary_skipped} secondary signal(s) -- every non-winning clause in a call \
         is stamped `escalate` unconditionally, so counting it reads far above the true \
         ask-level rate. See docs/issues.md, \"The ledger cannot be counted\"."
    );
    if no_primary > 0 {
        println!("{no_primary} request(s) carried no seq == 0 signal and were not counted.");
    }
    let fallback_total: u64 = by_model.values().map(|a| a.fallback).sum();
    if fallback_total > 0 {
        println!(
            "{fallback_total} ask(s) were scored WITHOUT per-command granularity: kaish could \
             not plan the command, so the hook scored the whole raw string as one clause. That \
             path fires ~16x more often than the plan path, and the fix is the kaish parse \
             failure, not the classifier."
        );
    }
    for (model, agg) in &by_model {
        if agg.asks < MIN_ASKS_FOR_A_RATE {
            println!(
                "WARNING: {model} has only {} ask(s) -- a small N is not a measurement \
                 (our v10 sample was 6 and must not be quoted as a rate).",
                agg.asks
            );
        }
    }
    Ok(())
}

// --- main ---------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // --measured never touches the corpus or the scorer -- it is an
    // offline read of a ledger dump.
    if let Some(path) = &cli.measured {
        return run_measured(path);
    }

    let corpus = corpus::corpus().context("kj::corpus::corpus() failed to build the probe corpus")?;

    if let Some(path) = &cli.dump_corpus {
        return run_dump_corpus(path, &corpus);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(cli.timeout))
        .build()
        .context("building the HTTP client")?;

    let models = fetch_models(&client, &cli.url).await?;
    let head = select_head(&models)?;
    print_head(&head);

    let cascade_url = format!("{}/v1/cascade", cli.url);

    if cli.aliases {
        let pairs = corpus::alias_pairs(&corpus);
        if pairs.is_empty() {
            bail!("alias_pairs() returned zero pairs -- corpus has no alias-pair coverage");
        }
        return run_aliases(&client, &cascade_url, &pairs).await;
    }

    let probes = flatten_corpus(&corpus);
    if probes.is_empty() {
        bail!("corpus is empty -- zero verbs and zero extras");
    }

    let rows = if cli.solo {
        if cli.json.is_some() {
            eprintln!(
                "note: --json is ignored under --solo -- solo makes one request per clause, \
                 so there is no single raw response to dump"
            );
        }
        score_solo(&client, &cascade_url, &probes).await?
    } else {
        let (rows, raw) = score_batched(&client, &cascade_url, &probes).await?;
        if let Some(json_path) = &cli.json {
            let pretty = serde_json::to_string_pretty(&raw).context("formatting the raw cascade response")?;
            std::fs::write(json_path, pretty).with_context(|| format!("writing {json_path:?}"))?;
        }
        rows
    };

    print_scored_table(&rows);
    print_escalate_summary(&rows);

    if let Some(floor) = cli.floor {
        print_floor(&rows, floor);
    }

    Ok(())
}
