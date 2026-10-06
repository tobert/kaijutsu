//! Answer and pool math, exactly as `docs/council-api.md` "Answers" states it.
//!
//! All arithmetic is float64 and every sum runs in request order, so a client
//! that recomputes a number agrees with a server within [`TOLERANCE`].
//! [`verify`] recomputes every derived number of a response from the reads'
//! `logprobs` and `mass`.

use std::fmt;

use indexmap::IndexMap;

use crate::council::wire::{
    DecisionRequest, DecisionResponse, PoolMethod, PoolWeights, PooledAnswer, ReadAnswer,
};

/// The largest absolute difference [`verify`] accepts.
pub const TOLERANCE: f64 = 1e-9;

/// Why numbers cannot be computed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MathError {
    /// No reads, or no options.
    #[error("nothing to compute over: {0}")]
    Empty(&'static str),
    /// A number is not finite.
    #[error("{0} is not finite")]
    NotFinite(&'static str),
    /// Reads do not have the same options.
    #[error("reads disagree on the number of options: {0} and {1}")]
    Ragged(usize, usize),
    /// The weights cannot pool anything.
    #[error("weights: {0}")]
    Weights(String),
}

/// `log Σ exp(x)`, summing in order.
pub fn log_sum_exp(xs: &[f64]) -> f64 {
    let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !max.is_finite() {
        return max;
    }
    let mut sum = 0.0;
    for x in xs {
        sum += (x - max).exp();
    }
    max + sum.ln()
}

/// The index of the largest value; a tie goes to the earlier index.
pub fn argmax(p: &[f64]) -> usize {
    let mut best = 0;
    for i in 1..p.len() {
        if p[i] > p[best] {
            best = i;
        }
    }
    best
}

/// The probability-weighted level number: `Σ k * p[k]`.
pub fn score(p: &[f64]) -> f64 {
    let mut s = 0.0;
    for (k, x) in p.iter().enumerate() {
        s += k as f64 * x;
    }
    s
}

/// A read's confidence: `exp(mass) * max(p)`.
pub fn confidence(mass: f64, p: &[f64]) -> f64 {
    mass.exp() * p[argmax(p)]
}

/// One read's numbers for one question, in option order.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// `logprobs[o] - mass`.
    pub log_probs: Vec<f64>,
    /// `exp(logprobs[o] - mass)`; sums to 1.
    pub probs: Vec<f64>,
    /// `log Σ exp(logprobs)`.
    pub mass: f64,
}

impl Row {
    /// Derives mass and probabilities from raw full-vocabulary log probabilities.
    pub fn from_logprobs(logprobs: &[f64]) -> Result<Row, MathError> {
        if logprobs.is_empty() {
            return Err(MathError::Empty("a read has no options"));
        }
        if logprobs.iter().any(|x| !x.is_finite()) {
            return Err(MathError::NotFinite("a logprob"));
        }
        let mass = log_sum_exp(logprobs);
        let log_probs: Vec<f64> = logprobs.iter().map(|x| x - mass).collect();
        let probs = log_probs.iter().map(|x| x.exp()).collect();
        Ok(Row { log_probs, probs, mass })
    }
}

/// How reads are weighted.
#[derive(Clone, Debug, PartialEq)]
pub enum WeightSpec {
    Uniform,
    /// Each read's `exp(mass)`.
    Mass,
    /// One caller-given weight per read.
    Given(Vec<f64>),
}

/// The result of pooling one question over its reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Pooled {
    /// The pooled probability of each option.
    pub probs: Vec<f64>,
    /// The normalized weight each read pooled with.
    pub weights: Vec<f64>,
    /// Every read has the same top option.
    pub agree: bool,
    /// The largest gap between two reads' probabilities for one option.
    pub spread: f64,
    /// The weighted mean of the reads' `exp(mass)`.
    pub mean_exp_mass: f64,
    /// With two or more reads, the pooled probabilities without each read;
    /// `None` where no weighted read remains.
    pub leave_one_out: Option<Vec<Option<Vec<f64>>>>,
}

impl Pooled {
    /// The pooled confidence: the top pooled probability times the mean `exp(mass)`.
    pub fn confidence(&self) -> f64 {
        self.probs[argmax(&self.probs)] * self.mean_exp_mass
    }
}

fn raw_weights(rows: &[Row], spec: &WeightSpec) -> Result<Vec<f64>, MathError> {
    match spec {
        WeightSpec::Uniform => Ok(vec![1.0; rows.len()]),
        WeightSpec::Mass => Ok(rows.iter().map(|r| r.mass.exp()).collect()),
        WeightSpec::Given(v) => {
            if v.len() != rows.len() {
                return Err(MathError::Weights(format!("{} values for {} reads", v.len(), rows.len())));
            }
            if v.iter().any(|x| !x.is_finite() || *x < 0.0) {
                return Err(MathError::Weights("values must be finite and at least 0".into()));
            }
            Ok(v.clone())
        }
    }
}

fn sum_in_order(xs: &[f64]) -> f64 {
    let mut t = 0.0;
    for x in xs {
        t += x;
    }
    t
}

fn normalize(raw: &[f64]) -> Result<Vec<f64>, MathError> {
    let total = sum_in_order(raw);
    if !(total.is_finite() && total > 0.0) {
        return Err(MathError::Weights(format!("the weights sum to {total}: nothing to pool")));
    }
    Ok(raw.iter().map(|x| x / total).collect())
}

fn combine(rows: &[&Row], w: &[f64], method: PoolMethod) -> Vec<f64> {
    let k = rows[0].probs.len();
    let mut acc = vec![0.0; k];
    match method {
        PoolMethod::Linear => {
            for (row, wc) in rows.iter().zip(w) {
                for (a, p) in acc.iter_mut().zip(&row.probs) {
                    *a += wc * p;
                }
            }
            let total = sum_in_order(&acc);
            acc.iter().map(|x| x / total).collect()
        }
        PoolMethod::Loglinear => {
            for (row, wc) in rows.iter().zip(w) {
                for (a, l) in acc.iter_mut().zip(&row.log_probs) {
                    *a += wc * l;
                }
            }
            let max = acc.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let e: Vec<f64> = acc.iter().map(|x| (x - max).exp()).collect();
            let total = sum_in_order(&e);
            e.iter().map(|x| x / total).collect()
        }
    }
}

/// Pools one question's reads, in request order.
pub fn pool(rows: &[Row], method: PoolMethod, weights: &WeightSpec) -> Result<Pooled, MathError> {
    if rows.is_empty() {
        return Err(MathError::Empty("no reads"));
    }
    let k = rows[0].probs.len();
    if let Some(r) = rows.iter().find(|r| r.probs.len() != k) {
        return Err(MathError::Ragged(k, r.probs.len()));
    }
    let raw = raw_weights(rows, weights)?;
    let w = normalize(&raw)?;
    let refs: Vec<&Row> = rows.iter().collect();
    let probs = combine(&refs, &w, method);

    let tops: Vec<usize> = rows.iter().map(|r| argmax(&r.probs)).collect();
    let agree = tops.iter().all(|t| *t == tops[0]);
    let mut spread = 0.0_f64;
    for o in 0..k {
        let mut hi = rows[0].probs[o];
        let mut lo = hi;
        for r in &rows[1..] {
            hi = hi.max(r.probs[o]);
            lo = lo.min(r.probs[o]);
        }
        spread = spread.max(hi - lo);
    }
    let mut mean_exp_mass = 0.0;
    for (wc, r) in w.iter().zip(rows) {
        mean_exp_mass += wc * r.mass.exp();
    }

    let leave_one_out = if rows.len() >= 2 {
        let mut out = Vec::with_capacity(rows.len());
        for i in 0..rows.len() {
            let keep: Vec<usize> = (0..rows.len()).filter(|j| *j != i).collect();
            let kept_raw: Vec<f64> = keep.iter().map(|j| raw[*j]).collect();
            let left = sum_in_order(&kept_raw);
            if left.is_nan() || left <= 0.0 {
                out.push(None);
                continue;
            }
            let kw = normalize(&kept_raw)?;
            let kept_rows: Vec<&Row> = keep.iter().map(|j| &rows[*j]).collect();
            out.push(Some(combine(&kept_rows, &kw, method)));
        }
        Some(out)
    } else {
        None
    };
    Ok(Pooled { probs, weights: w, agree, spread, mean_exp_mass, leave_one_out })
}

/// The first field of a response that disagrees with a recomputation.
#[derive(Clone, Debug, PartialEq)]
pub struct Mismatch {
    /// A path such as `answers.verdict.probabilities.ask`.
    pub field: String,
    /// What the recomputation gives.
    pub expected: String,
    /// What the response says.
    pub actual: String,
}

impl Mismatch {
    fn new(field: impl Into<String>, expected: impl fmt::Display, actual: impl fmt::Display) -> Self {
        Mismatch { field: field.into(), expected: expected.to_string(), actual: actual.to_string() }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: expected {}, got {}", self.field, self.expected, self.actual)
    }
}

impl std::error::Error for Mismatch {}

type Check = Result<(), Mismatch>;

fn close(field: &str, expected: f64, actual: f64) -> Check {
    if actual.is_finite() && (expected - actual).abs() <= TOLERANCE {
        Ok(())
    } else {
        Err(Mismatch::new(field, expected, actual))
    }
}

fn same_keys<V>(field: &str, expected: &[String], actual: &IndexMap<String, V>) -> Result<(), Mismatch> {
    let got: Vec<&String> = actual.keys().collect();
    if got.len() == expected.len() && got.iter().zip(expected).all(|(a, b)| *a == b) {
        Ok(())
    } else {
        Err(Mismatch::new(field, format!("keys {expected:?}"), format!("keys {got:?}")))
    }
}

fn check_probs(field: &str, keys: &[String], expected: &[f64], actual: &IndexMap<String, f64>) -> Check {
    same_keys(field, keys, actual)?;
    for (i, (k, v)) in actual.iter().enumerate() {
        close(&format!("{field}.{k}"), expected[i], *v)?;
    }
    Ok(())
}

/// The option an answer must name: the first whose probability is within the
/// tolerance of the largest. `claimed` is acceptable when its own probability is.
fn check_choice(field: &str, keys: &[String], p: &[f64], claimed: &str) -> Check {
    let top = p[argmax(p)];
    match keys.iter().position(|k| k == claimed) {
        Some(i) if p[i] >= top - TOLERANCE => Ok(()),
        _ => Err(Mismatch::new(field, keys[argmax(p)].clone(), claimed)),
    }
}

fn legend_keys(n: usize) -> Vec<String> {
    (0..n).map(|k| k.to_string()).collect()
}

/// One read's answer as option keys and raw log probabilities.
fn raw_of(a: &ReadAnswer) -> (&'static str, Vec<String>, Vec<f64>) {
    match a {
        ReadAnswer::Choice(c) => ("choice", c.logprobs.keys().cloned().collect(), c.logprobs.values().copied().collect()),
        ReadAnswer::Score(c) => ("score", c.logprobs.keys().cloned().collect(), c.logprobs.values().copied().collect()),
        ReadAnswer::Noul(c) => {
            let get = |k: &str| c.logprobs.get(k).copied().unwrap_or(f64::NAN);
            ("noul", vec!["yes".into(), "no".into()], vec![get("yes"), get("no")])
        }
    }
}

fn check_read(path: &str, a: &ReadAnswer, row: &Row, keys: &[String]) -> Check {
    match a {
        ReadAnswer::Choice(c) => {
            close(&format!("{path}.mass"), row.mass, c.mass)?;
            check_probs(&format!("{path}.probabilities"), keys, &row.probs, &c.probabilities)?;
            close(&format!("{path}.confidence"), confidence(row.mass, &row.probs), c.confidence)?;
            check_choice(&format!("{path}.choice"), keys, &row.probs, &c.choice)
        }
        ReadAnswer::Score(c) => {
            close(&format!("{path}.mass"), row.mass, c.mass)?;
            check_probs(&format!("{path}.probabilities"), keys, &row.probs, &c.probabilities)?;
            close(&format!("{path}.confidence"), confidence(row.mass, &row.probs), c.confidence)?;
            close(&format!("{path}.score"), score(&row.probs), c.score)?;
            same_keys(&format!("{path}.legend"), &legend_keys(keys.len()), &c.legend)
        }
        ReadAnswer::Noul(c) => {
            if c.logprobs.len() != 2 || !c.logprobs.contains_key("yes") || !c.logprobs.contains_key("no") {
                return Err(Mismatch::new(
                    format!("{path}.logprobs"),
                    "keys yes and no",
                    format!("keys {:?}", c.logprobs.keys().collect::<Vec<_>>()),
                ));
            }
            close(&format!("{path}.mass"), row.mass, c.mass)?;
            close(&format!("{path}.noul"), row.probs[0], c.noul)
        }
    }
}

fn check_loo(
    field: &str,
    ids: &[Option<&str>],
    expected: &Option<Vec<Option<Vec<f64>>>>,
    keys: &[String],
    actual: Option<LooActual<'_>>,
) -> Check {
    let Some(actual) = actual else { return Ok(()) };
    let Some(expected) = expected else {
        return Err(Mismatch::new(field, "absent with fewer than two reads", "present"));
    };
    let ids: Vec<&str> = match ids.iter().copied().collect::<Option<Vec<_>>>() {
        Some(v) => v,
        None => return Err(Mismatch::new(field, "reads naming their contexts", "a read with context null")),
    };
    let got_keys: Vec<&str> = match &actual {
        LooActual::Probs(m) => m.keys().map(String::as_str).collect(),
        LooActual::Yes(m) => m.keys().map(String::as_str).collect(),
    };
    if got_keys != ids {
        return Err(Mismatch::new(field, format!("keys {ids:?}"), format!("keys {got_keys:?}")));
    }
    for (i, id) in ids.iter().enumerate() {
        let f = format!("{field}.{id}");
        match (&actual, &expected[i]) {
            (LooActual::Probs(m), Some(p)) => match m.get_index(i).and_then(|(_, v)| v.as_ref()) {
                Some(got) => check_probs(&f, keys, p, got)?,
                None => return Err(Mismatch::new(f, "probabilities", "null")),
            },
            (LooActual::Probs(m), None) => {
                if m.get_index(i).is_some_and(|(_, v)| v.is_some()) {
                    return Err(Mismatch::new(f, "null", "probabilities"));
                }
            }
            (LooActual::Yes(m), Some(p)) => match m.get_index(i).and_then(|(_, v)| *v) {
                Some(got) => close(&f, p[0], got)?,
                None => return Err(Mismatch::new(f, p[0], "null")),
            },
            (LooActual::Yes(m), None) => {
                if m.get_index(i).is_some_and(|(_, v)| v.is_some()) {
                    return Err(Mismatch::new(f, "null", "a number"));
                }
            }
        }
    }
    Ok(())
}

enum LooActual<'a> {
    Probs(&'a IndexMap<String, Option<IndexMap<String, f64>>>),
    Yes(&'a IndexMap<String, Option<f64>>),
}

fn math_mismatch(field: &str, e: MathError) -> Mismatch {
    Mismatch::new(field, "numbers that can be pooled", e)
}

/// Recomputes every derived number of `response` from its reads' `logprobs`
/// and the request's pool settings, and returns the first that disagrees by
/// more than [`TOLERANCE`]. Raw `logprobs` are the server's and are not checked.
///
/// A `choice` is accepted when its probability is within the tolerance of the
/// largest, so a float tie that rounds the other way is not a mismatch.
pub fn verify(response: &DecisionResponse, request: &DecisionRequest) -> Result<(), Mismatch> {
    let settings = request.pool.clone().unwrap_or_default();
    let method = settings.method.unwrap_or(PoolMethod::Linear);
    let weights_kind = settings.weights.unwrap_or(PoolWeights::Uniform);
    if response.pool.method != method {
        return Err(Mismatch::new("pool.method", format!("{method:?}"), format!("{:?}", response.pool.method)));
    }
    if response.pool.weights != weights_kind {
        return Err(Mismatch::new("pool.weights", format!("{weights_kind:?}"), format!("{:?}", response.pool.weights)));
    }
    let spec = match weights_kind {
        PoolWeights::Uniform => WeightSpec::Uniform,
        PoolWeights::Mass => WeightSpec::Mass,
        PoolWeights::Given => WeightSpec::Given(settings.values.clone().unwrap_or_default()),
    };

    let wanted: Vec<&str> = request.contexts.iter().flatten().map(|c| c.id.as_str()).collect();
    let ids: Vec<Option<&str>> = response.reads.iter().map(|r| r.context.as_deref()).collect();
    if wanted.is_empty() {
        if ids != [None] {
            return Err(Mismatch::new("reads", "one read with context null", format!("{} reads", ids.len())));
        }
    } else if ids.iter().copied().collect::<Option<Vec<_>>>().as_deref() != Some(&wanted[..]) {
        return Err(Mismatch::new("reads", format!("contexts {wanted:?} in order"), format!("contexts {ids:?}")));
    }

    for read in &response.reads {
        if let Some(q) = read.answers.keys().find(|q| !response.answers.contains_key(*q)) {
            return Err(Mismatch::new(format!("answers.{q}"), "a pooled answer", "absent"));
        }
    }

    for (q, pooled) in &response.answers {
        let base = format!("answers.{q}");
        let mut rows = Vec::with_capacity(response.reads.len());
        let mut keys: Vec<String> = Vec::new();
        let mut kind = "";
        for (i, read) in response.reads.iter().enumerate() {
            let path = format!("reads[{i}].answers.{q}");
            let Some(a) = read.answers.get(q) else {
                return Err(Mismatch::new(path, "an answer", "absent"));
            };
            let (k, ks, lp) = raw_of(a);
            if i == 0 {
                kind = k;
                keys = ks;
            } else if k != kind || ks != keys {
                return Err(Mismatch::new(path, format!("a {kind} answer with keys {keys:?}"), format!("a {k} answer with keys {ks:?}")));
            }
            let row = Row::from_logprobs(&lp).map_err(|e| math_mismatch(&format!("{path}.logprobs"), e))?;
            check_read(&path, a, &row, &keys)?;
            rows.push(row);
        }
        let p = pool(&rows, method, &spec).map_err(|e| math_mismatch(&base, e))?;

        match response.pool.normalized.get(q) {
            Some(n) if n.len() == p.weights.len() => {
                for (i, (e, a)) in p.weights.iter().zip(n).enumerate() {
                    close(&format!("pool.normalized.{q}[{i}]"), *e, *a)?;
                }
            }
            Some(n) => return Err(Mismatch::new(format!("pool.normalized.{q}"), format!("{} weights", p.weights.len()), format!("{} weights", n.len()))),
            None => return Err(Mismatch::new(format!("pool.normalized.{q}"), "weights", "absent")),
        }

        match pooled {
            PooledAnswer::Choice(a) => {
                if kind != "choice" {
                    return Err(Mismatch::new(format!("{base}.type"), kind, "choice"));
                }
                check_probs(&format!("{base}.probabilities"), &keys, &p.probs, &a.probabilities)?;
                close(&format!("{base}.confidence"), p.confidence(), a.confidence)?;
                check_choice(&format!("{base}.choice"), &keys, &p.probs, &a.choice)?;
                check_agreement(&base, &p, a.agree, a.spread)?;
                check_loo(&format!("{base}.leave_one_out"), &ids, &p.leave_one_out, &keys, a.leave_one_out.as_ref().map(LooActual::Probs))?;
            }
            PooledAnswer::Score(a) => {
                if kind != "score" {
                    return Err(Mismatch::new(format!("{base}.type"), kind, "score"));
                }
                check_probs(&format!("{base}.probabilities"), &keys, &p.probs, &a.probabilities)?;
                close(&format!("{base}.confidence"), p.confidence(), a.confidence)?;
                close(&format!("{base}.score"), score(&p.probs), a.score)?;
                same_keys(&format!("{base}.legend"), &legend_keys(keys.len()), &a.legend)?;
                check_agreement(&base, &p, a.agree, a.spread)?;
                check_loo(&format!("{base}.leave_one_out"), &ids, &p.leave_one_out, &keys, a.leave_one_out.as_ref().map(LooActual::Probs))?;
            }
            PooledAnswer::Noul(a) => {
                if kind != "noul" {
                    return Err(Mismatch::new(format!("{base}.type"), kind, "noul"));
                }
                close(&format!("{base}.noul"), p.probs[0], a.noul)?;
                check_agreement(&base, &p, a.agree, a.spread)?;
                check_loo(&format!("{base}.leave_one_out"), &ids, &p.leave_one_out, &keys, a.leave_one_out.as_ref().map(LooActual::Yes))?;
            }
        }
    }
    Ok(())
}

fn check_agreement(base: &str, p: &Pooled, agree: bool, spread: f64) -> Check {
    close(&format!("{base}.spread"), p.spread, spread)?;
    if p.agree != agree {
        return Err(Mismatch::new(format!("{base}.agree"), p.agree, agree));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REQUEST: &str = include_str!("../../tests/fixtures/decision_request.json");
    const RESPONSE: &str = include_str!("../../tests/fixtures/decision_response.json");
    // Vectors below come from the megakernel's service/council.py answer() and
    // pooled() on the same float32 letter logits.
    const R1: [f64; 3] = [-4.0, -0.25, -3.5];
    const R2: [f64; 3] = [-2.0, -1.5, -6.0];
    const R3: [f64; 3] = [-4.75, -1.25, -0.75];

    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "{a} vs {b}");
    }

    fn near_all(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
        for (x, y) in a.iter().zip(b) {
            near(*x, *y);
        }
    }

    fn rows(lps: &[&[f64]]) -> Vec<Row> {
        lps.iter().map(|l| Row::from_logprobs(l).unwrap()).collect()
    }

    #[test]
    fn a_read_derives_probabilities_mass_and_confidence() {
        let r = Row::from_logprobs(&R1).unwrap();
        near(r.mass, -0.1895712056504122);
        near_all(&r.probs, &[0.02213868397888885, 0.941360796839809, 0.0365005191813022]);
        near(sum_in_order(&r.probs), 1.0);
        near(confidence(r.mass, &r.probs), 0.7788007830714049);
        for (o, p) in r.probs.iter().enumerate() {
            near(*p, (R1[o] - r.mass).exp());
        }
        assert_eq!(argmax(&r.probs), 1);
    }

    #[test]
    fn a_tie_goes_to_the_earlier_option() {
        assert_eq!(argmax(&[0.25, 0.5, 0.5, 0.0]), 1);
        let r = Row::from_logprobs(&[-1.0, -1.0, -3.0]).unwrap();
        assert_eq!(argmax(&r.probs), 0);
    }

    #[test]
    fn score_is_the_probability_weighted_level_number() {
        near(score(&[0.5, 0.25, 0.25]), 0.75);
        let r = Row::from_logprobs(&R1).unwrap();
        near(score(&r.probs), 1.0143618352024133);
    }

    #[test]
    fn loglinear_mass_pool_matches_the_megakernel() {
        let rs = rows(&[&R1, &R2, &R3]);
        let p = pool(&rs, PoolMethod::Loglinear, &WeightSpec::Mass).unwrap();
        near_all(&p.probs, &[0.04000403396307743, 0.8464704434588431, 0.11352552257807932]);
        near_all(&p.weights, &[0.4230094197818746, 0.18455245608721646, 0.3924381241309089]);
        near(p.confidence(), 0.6075795333411728);
        near(p.spread, 0.6085754162330479);
        assert!(!p.agree);
        let loo = p.leave_one_out.unwrap();
        near_all(loo[0].as_ref().unwrap(), &[0.055831953686386826, 0.7082420080753632, 0.23592603823825006]);
        near_all(loo[1].as_ref().unwrap(), &[0.021014481235269777, 0.7922666847753634, 0.18671883398936676]);
        near_all(loo[2].as_ref().unwrap(), &[0.05792360471276348, 0.9177342559688639, 0.024342139318372518]);
    }

    #[test]
    fn weights_are_exp_mass_normalized() {
        let rs = rows(&[&R1, &R2, &R3]);
        let p = pool(&rs, PoolMethod::Linear, &WeightSpec::Mass).unwrap();
        let raw: Vec<f64> = rs.iter().map(|r| r.mass.exp()).collect();
        let total = raw[0] + raw[1] + raw[2];
        near_all(&p.weights, &[raw[0] / total, raw[1] / total, raw[2] / total]);
        near(p.mean_exp_mass, (raw[0] * raw[0] + raw[1] * raw[1] + raw[2] * raw[2]) / total);
    }

    #[test]
    fn linear_given_pool_matches_the_megakernel_and_a_zero_weight_read_is_skipped() {
        let rs = rows(&[&R1, &R2, &R3]);
        let p = pool(&rs, PoolMethod::Linear, &WeightSpec::Given(vec![1.0, 0.0, 3.0])).unwrap();
        near_all(&p.probs, &[0.013988842431352812, 0.5153039072702417, 0.47070725029840554]);
        near_all(&p.weights, &[0.25, 0.0, 0.75]);
        near(p.confidence(), 0.4032102270437644);
        let loo = p.leave_one_out.unwrap();
        near_all(loo[0].as_ref().unwrap(), &[0.011272228582174132, 0.37328494408038587, 0.61544282733744]);
        near_all(loo[1].as_ref().unwrap(), &[0.013988842431352812, 0.5153039072702417, 0.47070725029840554]);
        near_all(loo[2].as_ref().unwrap(), &[0.02213868397888885, 0.941360796839809, 0.0365005191813022]);
    }

    #[test]
    fn leave_one_out_is_none_when_no_weighted_read_remains() {
        let rs = rows(&[&R1, &R2]);
        let p = pool(&rs, PoolMethod::Linear, &WeightSpec::Given(vec![1.0, 0.0])).unwrap();
        near_all(&p.probs, &[0.02213868397888885, 0.941360796839809, 0.0365005191813022]);
        assert!(p.agree);
        near(p.spread, 0.3528092578379926);
        let loo = p.leave_one_out.unwrap();
        assert!(loo[0].is_none());
        near_all(loo[1].as_ref().unwrap(), &[0.02213868397888885, 0.941360796839809, 0.0365005191813022]);
    }

    #[test]
    fn linear_uniform_score_pool_matches_the_megakernel() {
        let rs = rows(&[&R1, &R2, &R3]);
        let p = pool(&rs, PoolMethod::Linear, &WeightSpec::Uniform).unwrap();
        near_all(&p.probs, &[0.13611961812598147, 0.6442767959996405, 0.21960358587437812]);
        near(score(&p.probs), 1.0834839677483967);
        near(p.confidence(), 0.4200214486105548);
    }

    #[test]
    fn noul_pools_over_yes_and_no() {
        let rs = rows(&[&[-0.25, -1.75], &[-2.5, -0.5]]);
        near(rs[0].probs[0], 0.8175744761936437);
        near(rs[1].probs[0], 0.11920292202211753);
        let p = pool(&rs, PoolMethod::Loglinear, &WeightSpec::Mass).unwrap();
        near(p.probs[0], 0.5078641874521359);
        near(p.spread, 0.6983715541715261);
        near_all(&p.weights, &[0.5804169554673861, 0.41958304453261386]);
        let loo = p.leave_one_out.unwrap();
        near(loo[0].as_ref().unwrap()[0], 0.11920292202211755);
        near(loo[1].as_ref().unwrap()[0], 0.8175744761936437);
    }

    #[test]
    fn one_read_agrees_with_zero_spread_and_no_leave_one_out() {
        let rs = rows(&[&R1]);
        let p = pool(&rs, PoolMethod::Loglinear, &WeightSpec::Mass).unwrap();
        assert!(p.agree);
        assert_eq!(p.spread, 0.0);
        assert!(p.leave_one_out.is_none());
        near_all(&p.probs, &rs[0].probs);
        near(p.confidence(), confidence(rs[0].mass, &rs[0].probs));
    }

    #[test]
    fn a_loglinear_pool_lets_one_read_veto_an_option() {
        let rs = rows(&[&[-0.1, -2.5, -9.0], &[-9.0, -2.5, -0.1]]);
        let lin = pool(&rs, PoolMethod::Linear, &WeightSpec::Uniform).unwrap();
        let log = pool(&rs, PoolMethod::Loglinear, &WeightSpec::Uniform).unwrap();
        assert!(lin.probs[0] > 0.4 && log.probs[0] < 0.2, "{:?} {:?}", lin.probs, log.probs);
        assert_eq!(argmax(&log.probs), 1);
    }

    #[test]
    fn pooling_that_cannot_happen_is_an_error() {
        let rs = rows(&[&R1, &R2]);
        assert!(pool(&[], PoolMethod::Linear, &WeightSpec::Uniform).is_err());
        assert!(pool(&rs, PoolMethod::Linear, &WeightSpec::Given(vec![0.0, 0.0])).is_err());
        assert!(pool(&rs, PoolMethod::Linear, &WeightSpec::Given(vec![1.0])).is_err());
        assert!(pool(&rs, PoolMethod::Linear, &WeightSpec::Given(vec![1.0, -1.0])).is_err());
        let ragged = rows(&[&R1, &[-1.0, -1.0]]);
        assert!(matches!(pool(&ragged, PoolMethod::Linear, &WeightSpec::Uniform), Err(MathError::Ragged(3, 2))));
        // exp(mass) underflows to 0 for both reads.
        let dead = rows(&[&[-2000.0, -2001.0], &[-2000.0, -2001.0]]);
        let masses: Vec<f64> = dead.iter().map(|r| r.mass).collect();
        assert!(masses.iter().all(|m| *m < -1990.0));
        assert!(pool(&dead, PoolMethod::Linear, &WeightSpec::Mass).is_err());
        assert!(Row::from_logprobs(&[f64::NEG_INFINITY, -1.0]).is_err());
    }

    // ---- verify

    fn fixture() -> (DecisionResponse, DecisionRequest) {
        (serde_json::from_str(RESPONSE).unwrap(), serde_json::from_str(REQUEST).unwrap())
    }

    #[test]
    fn verify_accepts_the_megakernel_fixture() {
        let (resp, req) = fixture();
        verify(&resp, &req).unwrap();
    }

    fn edit(pointer: &str, value: serde_json::Value) -> Mismatch {
        let (_, req) = fixture();
        let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        *v.pointer_mut(pointer).unwrap_or_else(|| panic!("{pointer}")) = value;
        let resp: DecisionResponse = serde_json::from_value(v).unwrap();
        verify(&resp, &req).expect_err(pointer)
    }

    #[test]
    fn verify_catches_a_wrong_pooled_number() {
        let m = edit("/answers/verdict/probabilities/ask", json!(0.5));
        assert_eq!(m.field, "answers.verdict.probabilities.ask");
        assert!(m.to_string().contains("expected"), "{m}");
    }

    #[test]
    fn verify_names_the_first_field_that_disagrees() {
        let (_, req) = fixture();
        let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        v["answers"]["undo"]["confidence"] = json!(0.0);
        v["answers"]["verdict"]["spread"] = json!(0.0);
        let resp: DecisionResponse = serde_json::from_value(v).unwrap();
        assert_eq!(verify(&resp, &req).unwrap_err().field, "answers.undo.confidence");
    }

    #[test]
    fn verify_catches_each_derived_number() {
        for (pointer, value, field) in [
            ("/reads/0/answers/verdict/mass", json!(-0.5), "reads[0].answers.verdict.mass"),
            ("/reads/1/answers/undo/probabilities/1", json!(0.9), "reads[1].answers.undo.probabilities.1"),
            ("/reads/0/answers/verdict/confidence", json!(0.99), "reads[0].answers.verdict.confidence"),
            ("/reads/0/answers/verdict/choice", json!("allow"), "reads[0].answers.verdict.choice"),
            ("/reads/0/answers/undo/score", json!(0.0), "reads[0].answers.undo.score"),
            ("/answers/verdict/choice", json!("allow"), "answers.verdict.choice"),
            ("/answers/verdict/confidence", json!(0.99), "answers.verdict.confidence"),
            ("/answers/undo/score", json!(0.0), "answers.undo.score"),
            ("/answers/undo/spread", json!(0.0), "answers.undo.spread"),
            ("/answers/verdict/agree", json!(true), "answers.verdict.agree"),
            ("/answers/verdict/leave_one_out/0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b11/ask", json!(0.0), "answers.verdict.leave_one_out.0199b3c4-6c1e-7a2b-9f00-3e5d1c2a7b11.ask"),
            ("/pool/normalized/verdict/0", json!(0.5), "pool.normalized.verdict[0]"),
            ("/pool/method", json!("linear"), "pool.method"),
            ("/pool/weights", json!("uniform"), "pool.weights"),
        ] {
            assert_eq!(edit(pointer, value).field, field, "{pointer}");
        }
    }

    #[test]
    fn verify_accepts_noise_inside_the_tolerance_and_rejects_it_outside() {
        let (resp, req) = fixture();
        let mut v: serde_json::Value = serde_json::from_str(RESPONSE).unwrap();
        let ask = v["answers"]["verdict"]["probabilities"]["ask"].as_f64().unwrap();
        v["answers"]["verdict"]["probabilities"]["ask"] = json!(ask + 5e-10);
        verify(&serde_json::from_value(v.clone()).unwrap(), &req).unwrap();
        v["answers"]["verdict"]["probabilities"]["ask"] = json!(ask + 5e-9);
        assert!(verify(&serde_json::from_value(v).unwrap(), &req).is_err());
        verify(&resp, &req).unwrap();
    }

    #[test]
    fn verify_rejects_reads_that_do_not_match_the_request() {
        let (resp, mut req) = fixture();
        req.contexts.as_mut().unwrap().reverse();
        assert_eq!(verify(&resp, &req).unwrap_err().field, "reads");
        let (mut resp, req) = fixture();
        resp.reads.pop();
        assert_eq!(verify(&resp, &req).unwrap_err().field, "reads");
    }

    #[test]
    fn verify_rejects_a_missing_or_stray_answer() {
        let (mut resp, req) = fixture();
        resp.reads[1].answers.shift_remove("undo");
        assert_eq!(verify(&resp, &req).unwrap_err().field, "reads[1].answers.undo");
        let (mut resp, req) = fixture();
        resp.answers.shift_remove("undo");
        assert_eq!(verify(&resp, &req).unwrap_err().field, "answers.undo");
    }

    #[test]
    fn verify_checks_a_single_read_with_no_leave_one_out() {
        let (resp, _) = fixture();
        let mut one = resp.clone();
        one.reads.truncate(1);
        let ReadAnswer::Choice(c) = &one.reads[0].answers["verdict"] else { panic!() };
        let c = c.clone();
        one.answers.shift_remove("undo");
        one.reads[0].answers.shift_remove("undo");
        one.pool.normalized.shift_remove("undo");
        *one.pool.normalized.get_mut("verdict").unwrap() = vec![1.0];
        one.answers.insert(
            "verdict".into(),
            PooledAnswer::Choice(crate::council::wire::PooledChoice {
                choice: c.choice,
                probabilities: c.probabilities,
                confidence: c.confidence,
                agree: true,
                spread: 0.0,
                leave_one_out: None,
            }),
        );
        let mut req: DecisionRequest = serde_json::from_str(REQUEST).unwrap();
        req.contexts.as_mut().unwrap().truncate(1);
        verify(&one, &req).unwrap();
        let PooledAnswer::Choice(p) = one.answers.get_mut("verdict").unwrap() else { panic!() };
        p.leave_one_out = Some(Default::default());
        assert_eq!(verify(&one, &req).unwrap_err().field, "answers.verdict.leave_one_out");
    }

    #[test]
    fn verify_checks_noul_answers() {
        use crate::council::wire::{PooledNoul, ReadNoul};
        let (mut resp, mut req) = fixture();
        let lp = [[-0.25, -1.75], [-2.5, -0.5]];
        let rs: Vec<Row> = lp.iter().map(|l| Row::from_logprobs(l).unwrap()).collect();
        let p = pool(&rs, PoolMethod::Loglinear, &WeightSpec::Mass).unwrap();
        resp.answers.clear();
        for (i, read) in resp.reads.iter_mut().enumerate() {
            read.answers.clear();
            read.answers.insert("risky".into(), ReadAnswer::Noul(ReadNoul {
                noul: rs[i].probs[0],
                logprobs: IndexMap::from([("yes".to_string(), lp[i][0]), ("no".to_string(), lp[i][1])]),
                mass: rs[i].mass,
            }));
        }
        let loo = p.leave_one_out.clone().unwrap();
        let ids: Vec<String> = resp.reads.iter().map(|r| r.context.clone().unwrap()).collect();
        resp.answers.insert("risky".into(), PooledAnswer::Noul(PooledNoul {
            noul: p.probs[0],
            agree: p.agree,
            spread: p.spread,
            leave_one_out: Some(ids.iter().cloned().zip(loo.iter().map(|l| l.as_ref().map(|v| v[0]))).collect()),
        }));
        resp.pool.normalized.clear();
        resp.pool.normalized.insert("risky".into(), p.weights.clone());
        req.pool = Some(crate::council::wire::Pool { method: Some(PoolMethod::Loglinear), weights: Some(PoolWeights::Mass), values: None });
        verify(&resp, &req).unwrap();
        let PooledAnswer::Noul(n) = resp.answers.get_mut("risky").unwrap() else { panic!() };
        n.noul += 0.01;
        assert_eq!(verify(&resp, &req).unwrap_err().field, "answers.risky.noul");
    }
}
