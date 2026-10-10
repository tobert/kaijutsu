//! Durable record of council decisions (`docs/council.md`, "The record").
//!
//! [`insert_council_decision`] writes a decision and every child row in one
//! transaction. A failed child insert leaves no decision behind. Readers
//! return a decision with its children, newest first for the list forms.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use strum::EnumString;

use crate::error::{LedgerError, Result};
use crate::types::parse_enum;

/// How a council decision ended. A `Miss` carries its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CouncilOutcome {
    Allow,
    Ask,
    Report,
    /// Bumper mode: the council refused the action with guidance.
    Bump,
    Miss,
}

impl CouncilOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Report => "report",
            Self::Bump => "bump",
            Self::Miss => "miss",
        }
    }
}

/// The identity of the server that produced the numbers. A threshold fitted
/// under one identity does not carry to another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilServer {
    pub model: String,
    pub weight_hash: String,
    pub tokenizer_hash: String,
    pub template: String,
    pub engine: String,
}

/// The threshold the decision was judged against.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilThreshold {
    pub allow_at: f64,
    pub mass_floor: f64,
    pub require_agree: bool,
}

/// The pooled verdict's agreement: every read has the same top option, and
/// the largest gap between two reads' probabilities for one option.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilAgreement {
    pub agree: bool,
    pub spread: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilOption {
    pub option: String,
    pub logprob: f64,
    pub probability: f64,
}

/// One read's answer to a choice, score, or noul question. `confidence` is
/// `None` for noul.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilQuestion {
    pub question_id: String,
    pub mass: f64,
    pub confidence: Option<f64>,
    pub options: Vec<CouncilOption>,
}

/// The text one read wrote for a text question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilDescribed {
    pub question_id: String,
    pub body: String,
}

/// One read. Its position in the decision's `reads` is its `read_idx`.
/// `context_id` is `None` for a read of the spec alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilRead {
    pub context_id: Option<Vec<u8>>,
    pub snapshot: String,
    pub expected_head: Option<String>,
    pub rendered_sha256: String,
    pub questions: Vec<CouncilQuestion>,
    pub described: Vec<CouncilDescribed>,
}

/// The pooled probability of one option of one question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilPooled {
    pub question_id: String,
    pub option: String,
    pub probability: f64,
}

/// Where the server found control text, and the token it spelled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilControlText {
    pub location: String,
    pub token: String,
}

/// Everything `insert_council_decision` records. The decision id, the
/// timestamp, and the control-text count are the ledger's to assign.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewCouncilDecision {
    pub request_id: Option<String>,
    pub context_id: Vec<u8>,
    pub principal_id: Vec<u8>,
    pub submission_digest: String,
    pub spec_id: String,
    pub spec_name: String,
    pub server: CouncilServer,
    pub pool_method: String,
    pub pool_weights: String,
    pub threshold: Option<CouncilThreshold>,
    pub deadline_ms: i64,
    pub outcome: CouncilOutcome,
    pub miss_cause: Option<String>,
    pub agreement: Option<CouncilAgreement>,
    pub queue_ms: i64,
    pub ms: i64,
    /// The head of the house-rules context the decision read
    /// (`docs/council.md`, "House rules"); `None` when it read none.
    /// The read's row carries that context's id, derived from its text.
    #[serde(default)]
    pub house_rules_head: Option<String>,
    /// A bump's flavor: the non-pass option the council chose. Set for a
    /// `Bump` and for nothing else.
    #[serde(default)]
    pub bump_flavor: Option<String>,
    pub reads: Vec<CouncilRead>,
    pub pooled: Vec<CouncilPooled>,
    pub control_text: Vec<CouncilControlText>,
}

/// A stored decision with its children.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilDecision {
    pub decision_id: Vec<u8>,
    pub created_at: i64,
    pub control_text_hits: i64,
    #[serde(flatten)]
    pub decision: NewCouncilDecision,
}

fn invalid(message: impl Into<String>) -> LedgerError {
    LedgerError::InvalidCouncilDecision(message.into())
}

fn check_outcome_and_cause(d: &NewCouncilDecision) -> Result<()> {
    match (d.outcome, d.bump_flavor.as_deref()) {
        (CouncilOutcome::Bump, None) => return Err(invalid("a bump must carry its flavor")),
        (CouncilOutcome::Bump, Some("")) => return Err(invalid("a bump flavor must not be empty")),
        (CouncilOutcome::Bump, Some(_)) | (_, None) => {}
        (outcome, Some(_)) => {
            return Err(invalid(format!("only a bump carries a flavor, this outcome is {}", outcome.as_str())));
        }
    }
    match (d.outcome, d.miss_cause.as_deref()) {
        (CouncilOutcome::Miss, None) => Err(invalid("a miss must carry its cause")),
        (CouncilOutcome::Miss, Some("")) => Err(invalid("a miss cause must not be empty")),
        (CouncilOutcome::Miss, Some(_)) => Ok(()),
        (outcome, Some(_)) => Err(invalid(format!("only a miss carries a cause, this outcome is {}", outcome.as_str()))),
        (_, None) => Ok(()),
    }
}

/// Record one decision and all its children in a single transaction and
/// return its id (16 UUIDv7 bytes). Any failure rolls the whole decision
/// back. A `request_id` that names no ask is refused, whether or not the
/// connection enforces foreign keys.
pub fn insert_council_decision(conn: &Connection, d: &NewCouncilDecision) -> Result<Vec<u8>> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let decision_id = insert_council_decision_within(&tx, d)?;
    tx.commit()?;
    Ok(decision_id)
}

/// [`insert_council_decision`] for a caller that already holds a
/// transaction on `conn`, such as the one that creates the decision's ask.
/// Nothing commits here: the caller's commit or rollback takes the
/// decision with it.
pub fn insert_council_decision_within(conn: &Connection, d: &NewCouncilDecision) -> Result<Vec<u8>> {
    check_outcome_and_cause(d)?;
    let decision_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    if let Some(request_id) = &d.request_id {
        let exists: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM approvals WHERE request_id = ?1)",
            [request_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(LedgerError::NotFound(request_id.clone()));
        }
    }
    insert_rows(conn, &decision_id, d)?;
    Ok(decision_id)
}

fn insert_rows(tx: &Connection, decision_id: &[u8], d: &NewCouncilDecision) -> Result<()> {
    tx.execute(
        "INSERT INTO council_decisions (
            decision_id, request_id, context_id, principal_id, submission_digest, spec_id, spec_name,
            server_model, weight_hash, tokenizer_hash, template, engine, pool_method, pool_weights,
            allow_at, mass_floor, require_agree, deadline_ms, outcome, miss_cause, agree, spread,
            control_text_hits, queue_ms, ms, created_at, house_rules_head, bump_flavor
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                   ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
        params![
            decision_id,
            d.request_id,
            d.context_id,
            d.principal_id,
            d.submission_digest,
            d.spec_id,
            d.spec_name,
            d.server.model,
            d.server.weight_hash,
            d.server.tokenizer_hash,
            d.server.template,
            d.server.engine,
            d.pool_method,
            d.pool_weights,
            d.threshold.map(|t| t.allow_at),
            d.threshold.map(|t| t.mass_floor),
            d.threshold.map(|t| t.require_agree),
            d.deadline_ms,
            d.outcome.as_str(),
            d.miss_cause,
            d.agreement.map(|a| a.agree),
            d.agreement.map(|a| a.spread),
            d.control_text.len() as i64,
            d.queue_ms,
            d.ms,
            crate::time::now_millis(),
            d.house_rules_head,
            d.bump_flavor,
        ],
    )?;
    for (read_idx, read) in d.reads.iter().enumerate() {
        let read_idx = read_idx as i64;
        tx.execute(
            "INSERT INTO council_reads (decision_id, read_idx, context_id, snapshot, expected_head, rendered_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![decision_id, read_idx, read.context_id, read.snapshot, read.expected_head, read.rendered_sha256],
        )?;
        for q in &read.questions {
            tx.execute(
                "INSERT INTO council_read_questions (decision_id, read_idx, question_id, mass, confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![decision_id, read_idx, q.question_id, q.mass, q.confidence],
            )?;
            for o in &q.options {
                tx.execute(
                    "INSERT INTO council_read_options (decision_id, read_idx, question_id, option, logprob, probability)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![decision_id, read_idx, q.question_id, o.option, o.logprob, o.probability],
                )?;
            }
        }
        for t in &read.described {
            tx.execute(
                "INSERT INTO council_described (decision_id, read_idx, question_id, body) VALUES (?1, ?2, ?3, ?4)",
                params![decision_id, read_idx, t.question_id, t.body],
            )?;
        }
    }
    for p in &d.pooled {
        tx.execute(
            "INSERT INTO council_pooled (decision_id, question_id, option, probability) VALUES (?1, ?2, ?3, ?4)",
            params![decision_id, p.question_id, p.option, p.probability],
        )?;
    }
    for (seq, hit) in d.control_text.iter().enumerate() {
        tx.execute(
            "INSERT INTO council_control_text (decision_id, seq, location, token) VALUES (?1, ?2, ?3, ?4)",
            params![decision_id, seq as i64, hit.location, hit.token],
        )?;
    }
    Ok(())
}

const DECISION_COLUMNS: &str = "decision_id, request_id, context_id, principal_id, submission_digest, spec_id, \
    spec_name, server_model, weight_hash, tokenizer_hash, template, engine, pool_method, pool_weights, allow_at, \
    mass_floor, require_agree, deadline_ms, outcome, miss_cause, agree, spread, control_text_hits, queue_ms, ms, \
    created_at, house_rules_head, bump_flavor";

/// Load one decision with its reads, pooled probabilities, and control-text
/// hits. `None` when no decision has this id.
pub fn load_council_decision(conn: &Connection, decision_id: &[u8]) -> Result<Option<CouncilDecision>> {
    let head = conn
        .query_row(
            &format!("SELECT {DECISION_COLUMNS} FROM council_decisions WHERE decision_id = ?1"),
            [decision_id],
            decode_decision,
        )
        .optional()?;
    head.map(|head| head.finish(conn)).transpose()
}

/// Decisions that led to this ask, newest first, at most `limit`.
pub fn list_council_decisions_for_request(
    conn: &Connection, request_id: &str, limit: u32,
) -> Result<Vec<CouncilDecision>> {
    list(
        conn,
        &format!(
            "SELECT {DECISION_COLUMNS} FROM council_decisions WHERE request_id = ?1
             ORDER BY created_at DESC, decision_id DESC LIMIT ?2"
        ),
        params![request_id, limit],
    )
}

/// Decisions made for this context, newest first, at most `limit`.
pub fn list_council_decisions_for_context(
    conn: &Connection, context_id: &[u8], limit: u32,
) -> Result<Vec<CouncilDecision>> {
    list(
        conn,
        &format!(
            "SELECT {DECISION_COLUMNS} FROM council_decisions WHERE context_id = ?1
             ORDER BY created_at DESC, decision_id DESC LIMIT ?2"
        ),
        params![context_id, limit],
    )
}

/// The flavors of the bumps the council refused for this context's
/// submission, oldest first; the count of bumps is the length. The
/// submission's shell decision (its digest is the command's) is a refused
/// bump when it bumped, or when one of its programs did: its flavor is the
/// shell's own, else the first bumping program's. A refused bump has no ask:
/// a bump at the limit becomes an ask, and its decision links to that ask,
/// so it does not count again.
pub fn list_bump_flavors(conn: &Connection, context_id: &[u8], submission_digest: &str) -> Result<Vec<String>> {
    const PROGRAM_BUMP: &str = "SELECT pd.bump_flavor FROM council_programs p
        JOIN council_decisions pd ON pd.decision_id = p.program_decision_id
        WHERE p.decision_id = d.decision_id AND pd.outcome = 'bump' ORDER BY p.seq LIMIT 1";
    let mut stmt = conn.prepare(&format!(
        "SELECT CASE WHEN d.outcome = 'bump' THEN d.bump_flavor ELSE ({PROGRAM_BUMP}) END
         FROM council_decisions d
         WHERE d.context_id = ?1 AND d.submission_digest = ?2 AND d.request_id IS NULL
           AND (d.outcome = 'bump' OR EXISTS ({PROGRAM_BUMP}))
         ORDER BY d.created_at ASC, d.decision_id ASC"
    ))?;
    let flavors = stmt
        .query_map(params![context_id, submission_digest], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(flavors)
}

/// One submission the council judged for a seat, read from its shell
/// decision: when, whether the seat got it back as a bump, why, and the
/// pooled `undo` read when the spec asked it.
#[derive(Clone, Debug, PartialEq)]
pub struct CouncilSubmission {
    pub decision_id: Vec<u8>,
    pub context_id: Vec<u8>,
    pub created_at: i64,
    pub submission_digest: String,
    /// The shell decision's own outcome.
    pub outcome: CouncilOutcome,
    /// The seat got the submission back with nothing run and no ask: a
    /// bump, or in bump-only mode a miss, a control-text hit, or a program
    /// that did not pass.
    pub bumped: bool,
    /// What the bump is called: the shell's flavor, else the first bumping
    /// program's, else `unjudged`, `control_text`, or `unread` as the gate
    /// names them. `None` when not bumped.
    pub flavor: Option<String>,
    /// The pooled `undo` argmax and its probability.
    pub undo: Option<(String, f64)>,
}

/// Bumps in a row: the trailing run of bumped submissions, newest last.
#[derive(Clone, Debug, PartialEq)]
pub struct BumpStreak {
    pub count: usize,
    pub first_at: i64,
    pub last_at: i64,
    /// Each bump's flavor, oldest first.
    pub flavors: Vec<String>,
}

/// Which submissions [`list_council_submissions`] returns.
#[derive(Clone, Debug, Default)]
pub struct SubmissionFilter {
    pub context_id: Option<Vec<u8>>,
    pub since_ms: Option<i64>,
    pub limit: u32,
}

/// Shell decisions, newest first. A program's own decision is part of its
/// submission and is not listed. A submission is bumped when no ask links
/// to it and it did not pass: the gate records exactly those without an ask
/// (`kj/gate.rs`, `record_unlinked_council_decision`).
pub fn list_council_submissions(conn: &Connection, filter: &SubmissionFilter) -> Result<Vec<CouncilSubmission>> {
    const PROGRAM_UNPASSED: &str = "SELECT p.seq FROM council_programs p
        LEFT JOIN council_decisions pd ON pd.decision_id = p.program_decision_id
        WHERE p.decision_id = d.decision_id AND (p.unread_cause IS NOT NULL OR pd.outcome != 'allow')";
    let sql = format!(
        "SELECT d.decision_id, d.context_id, d.created_at, d.submission_digest, d.outcome,
                d.request_id IS NULL AND (d.outcome != 'allow' OR EXISTS ({PROGRAM_UNPASSED})),
                COALESCE(
                    d.bump_flavor,
                    (SELECT pd.bump_flavor FROM council_programs p
                       JOIN council_decisions pd ON pd.decision_id = p.program_decision_id
                      WHERE p.decision_id = d.decision_id AND pd.outcome = 'bump' ORDER BY p.seq LIMIT 1),
                    CASE d.outcome WHEN 'miss' THEN 'unjudged' WHEN 'ask' THEN 'control_text'
                                   WHEN 'report' THEN 'control_text' END,
                    (SELECT CASE WHEN p.unread_cause IS NOT NULL THEN 'unread' ELSE 'unjudged' END
                       FROM council_programs p
                       LEFT JOIN council_decisions pd ON pd.decision_id = p.program_decision_id
                      WHERE p.decision_id = d.decision_id AND (p.unread_cause IS NOT NULL OR pd.outcome != 'allow')
                      ORDER BY p.seq LIMIT 1)),
                (SELECT option FROM council_pooled
                  WHERE decision_id = d.decision_id AND question_id = 'undo'
                  ORDER BY probability DESC, option LIMIT 1),
                (SELECT MAX(probability) FROM council_pooled
                  WHERE decision_id = d.decision_id AND question_id = 'undo')
           FROM council_decisions d
          WHERE NOT EXISTS (SELECT 1 FROM council_programs p WHERE p.program_decision_id = d.decision_id)
            AND (?1 IS NULL OR d.context_id = ?1)
            AND (?2 IS NULL OR d.created_at >= ?2)
          ORDER BY d.created_at DESC, d.decision_id DESC
          LIMIT ?3"
    );
    let rows = conn
        .prepare(&sql)?
        .query_map(params![filter.context_id, filter.since_ms, filter.limit], |row| {
            let outcome: String = row.get(4)?;
            let bumped: bool = row.get(5)?;
            let undo = match (row.get::<_, Option<String>>(7)?, row.get::<_, Option<f64>>(8)?) {
                (Some(option), Some(p)) => Some((option, p)),
                _ => None,
            };
            Ok((
                CouncilSubmission {
                    decision_id: row.get(0)?,
                    context_id: row.get(1)?,
                    created_at: row.get(2)?,
                    submission_digest: row.get(3)?,
                    outcome: CouncilOutcome::Allow,
                    bumped,
                    flavor: if bumped { row.get(6)? } else { None },
                    undo,
                },
                outcome,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(mut submission, outcome)| {
            submission.outcome = parse_enum::<CouncilOutcome>("outcome", &outcome)?;
            Ok(submission)
        })
        .collect()
}

/// The trailing bump streak of `submissions` (newest first, as
/// [`list_council_submissions`] returns them): the bumps before the most
/// recent submission that was not bumped. `None` when the newest was not.
pub fn bump_streak(submissions: &[CouncilSubmission]) -> Option<BumpStreak> {
    let run: Vec<&CouncilSubmission> = submissions.iter().take_while(|s| s.bumped).collect();
    let (newest, oldest) = (run.first()?, run.last()?);
    Some(BumpStreak {
        count: run.len(),
        first_at: oldest.created_at,
        last_at: newest.created_at,
        flavors: run.iter().rev().map(|s| s.flavor.clone().unwrap_or_default()).collect(),
    })
}

fn list(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Vec<CouncilDecision>> {
    let heads = conn
        .prepare(sql)?
        .query_map(args, decode_decision)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    heads.into_iter().map(|head| head.finish(conn)).collect()
}

/// A decision row before its children are loaded.
struct DecisionHead {
    decision_id: Vec<u8>,
    created_at: i64,
    control_text_hits: i64,
    decision: NewCouncilDecision,
}

fn decode_decision(row: &Row<'_>) -> rusqlite::Result<DecisionHead> {
    let outcome_raw: String = row.get("outcome")?;
    let outcome = parse_enum::<CouncilOutcome>("outcome", &outcome_raw).map_err(|e| match e {
        LedgerError::Db(db) => db,
        other => rusqlite::Error::InvalidColumnType(0, other.to_string(), rusqlite::types::Type::Text),
    })?;
    let threshold = match (
        row.get::<_, Option<f64>>("allow_at")?,
        row.get::<_, Option<f64>>("mass_floor")?,
        row.get::<_, Option<bool>>("require_agree")?,
    ) {
        (Some(allow_at), Some(mass_floor), Some(require_agree)) => {
            Some(CouncilThreshold { allow_at, mass_floor, require_agree })
        }
        _ => None,
    };
    let agreement = match (row.get::<_, Option<bool>>("agree")?, row.get::<_, Option<f64>>("spread")?) {
        (Some(agree), Some(spread)) => Some(CouncilAgreement { agree, spread }),
        _ => None,
    };
    Ok(DecisionHead {
        decision_id: row.get("decision_id")?,
        created_at: row.get("created_at")?,
        control_text_hits: row.get("control_text_hits")?,
        decision: NewCouncilDecision {
            request_id: row.get("request_id")?,
            context_id: row.get("context_id")?,
            principal_id: row.get("principal_id")?,
            submission_digest: row.get("submission_digest")?,
            spec_id: row.get("spec_id")?,
            spec_name: row.get("spec_name")?,
            server: CouncilServer {
                model: row.get("server_model")?,
                weight_hash: row.get("weight_hash")?,
                tokenizer_hash: row.get("tokenizer_hash")?,
                template: row.get("template")?,
                engine: row.get("engine")?,
            },
            pool_method: row.get("pool_method")?,
            pool_weights: row.get("pool_weights")?,
            threshold,
            deadline_ms: row.get("deadline_ms")?,
            outcome,
            miss_cause: row.get("miss_cause")?,
            agreement,
            queue_ms: row.get("queue_ms")?,
            ms: row.get("ms")?,
            house_rules_head: row.get("house_rules_head")?,
            bump_flavor: row.get("bump_flavor")?,
            reads: Vec::new(),
            pooled: Vec::new(),
            control_text: Vec::new(),
        },
    })
}

impl DecisionHead {
    fn finish(mut self, conn: &Connection) -> Result<CouncilDecision> {
        let id = self.decision_id.as_slice();
        let mut reads = conn
            .prepare(
                "SELECT context_id, snapshot, expected_head, rendered_sha256 FROM council_reads
                 WHERE decision_id = ?1 ORDER BY read_idx",
            )?
            .query_map([id], |row| {
                Ok(CouncilRead {
                    context_id: row.get(0)?,
                    snapshot: row.get(1)?,
                    expected_head: row.get(2)?,
                    rendered_sha256: row.get(3)?,
                    questions: Vec::new(),
                    described: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Children come back in insertion order (rowid), which is the
        // order the caller listed them in.
        let mut questions = conn.prepare(
            "SELECT read_idx, question_id, mass, confidence FROM council_read_questions
             WHERE decision_id = ?1 ORDER BY read_idx, rowid",
        )?;
        let mut rows = questions.query([id])?;
        while let Some(row) = rows.next()? {
            let read_idx: i64 = row.get(0)?;
            let read = reads
                .get_mut(read_idx as usize)
                .ok_or_else(|| invalid(format!("question row for missing read {read_idx}")))?;
            read.questions.push(CouncilQuestion {
                question_id: row.get(1)?,
                mass: row.get(2)?,
                confidence: row.get(3)?,
                options: Vec::new(),
            });
        }
        let mut options = conn.prepare(
            "SELECT read_idx, question_id, option, logprob, probability FROM council_read_options
             WHERE decision_id = ?1 ORDER BY read_idx, rowid",
        )?;
        let mut rows = options.query([id])?;
        while let Some(row) = rows.next()? {
            let read_idx: i64 = row.get(0)?;
            let question_id: String = row.get(1)?;
            let question = reads
                .get_mut(read_idx as usize)
                .and_then(|r| r.questions.iter_mut().find(|q| q.question_id == question_id))
                .ok_or_else(|| invalid(format!("option row for missing question {question_id} in read {read_idx}")))?;
            question.options.push(CouncilOption { option: row.get(2)?, logprob: row.get(3)?, probability: row.get(4)? });
        }
        let mut described = conn.prepare(
            "SELECT read_idx, question_id, body FROM council_described WHERE decision_id = ?1
             ORDER BY read_idx, rowid",
        )?;
        let mut rows = described.query([id])?;
        while let Some(row) = rows.next()? {
            let read_idx: i64 = row.get(0)?;
            let read = reads
                .get_mut(read_idx as usize)
                .ok_or_else(|| invalid(format!("described row for missing read {read_idx}")))?;
            read.described.push(CouncilDescribed { question_id: row.get(1)?, body: row.get(2)? });
        }

        let pooled = conn
            .prepare(
                "SELECT question_id, option, probability FROM council_pooled WHERE decision_id = ?1
                 ORDER BY rowid",
            )?
            .query_map([id], |row| {
                Ok(CouncilPooled { question_id: row.get(0)?, option: row.get(1)?, probability: row.get(2)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let control_text = conn
            .prepare("SELECT location, token FROM council_control_text WHERE decision_id = ?1 ORDER BY seq")?
            .query_map([id], |row| Ok(CouncilControlText { location: row.get(0)?, token: row.get(1)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        self.decision.reads = reads;
        self.decision.pooled = pooled;
        self.decision.control_text = control_text;
        Ok(CouncilDecision {
            decision_id: self.decision_id,
            created_at: self.created_at,
            control_text_hits: self.control_text_hits,
            decision: self.decision,
        })
    }
}

/// One program a council-decided submission runs, as
/// [`insert_council_program_within`] records it under the shell decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilProgram {
    pub statement_idx: i64,
    pub command: String,
    pub language: String,
    pub path: Option<String>,
    pub sha256: Option<String>,
    pub imports_not_shown: Vec<String>,
    /// Why the text could not be read; `None` when it was judged.
    pub unread_cause: Option<String>,
    /// The program's own decision; `None` exactly when `unread_cause` is set.
    pub program_decision_id: Option<Vec<u8>>,
}

/// Record the programs `decision_id`'s submission runs, in order, inside
/// the caller's transaction.
pub fn insert_council_programs_within(conn: &Connection, decision_id: &[u8], programs: &[CouncilProgram]) -> Result<()> {
    for (seq, p) in programs.iter().enumerate() {
        if p.unread_cause.is_some() == p.program_decision_id.is_some() {
            return Err(invalid("a program carries either its decision or the reason its text was not read"));
        }
        if p.imports_not_shown.iter().any(|name| name.is_empty() || name.contains(char::is_whitespace)) {
            return Err(invalid("an import name is one word"));
        }
        let imports = p.imports_not_shown.join(" ");
        conn.execute(
            "INSERT INTO council_programs (decision_id, seq, statement_idx, command, language, path, sha256,
                                           imports_not_shown, unread_cause, program_decision_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                decision_id, seq as i64, p.statement_idx, p.command, p.language, p.path, p.sha256, imports,
                p.unread_cause, p.program_decision_id
            ],
        )?;
    }
    Ok(())
}

/// The programs recorded under `decision_id`, in submission order.
pub fn list_council_programs(conn: &Connection, decision_id: &[u8]) -> Result<Vec<CouncilProgram>> {
    let rows = conn
        .prepare(
            "SELECT statement_idx, command, language, path, sha256, imports_not_shown, unread_cause, program_decision_id
             FROM council_programs WHERE decision_id = ?1 ORDER BY seq",
        )?
        .query_map([decision_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(statement_idx, command, language, path, sha256, imports, unread_cause, program_decision_id)| {
            CouncilProgram {
                statement_idx,
                command,
                language,
                path,
                sha256,
                imports_not_shown: imports.split_whitespace().map(str::to_owned).collect(),
                unread_cause,
                program_decision_id,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{ASKING_CONTEXT, minimal_ask, open_memory};

    fn option(name: &str, probability: f64) -> CouncilOption {
        CouncilOption { option: name.into(), logprob: probability.ln() - 0.1, probability }
    }

    fn read(context: Option<&[u8]>, tag: &str) -> CouncilRead {
        CouncilRead {
            context_id: context.map(<[u8]>::to_vec),
            snapshot: format!("snap-{tag}"),
            expected_head: Some(format!("head-{tag}")),
            rendered_sha256: format!("sha-{tag}"),
            questions: vec![
                CouncilQuestion {
                    question_id: "undo".into(),
                    mass: -0.25,
                    confidence: Some(0.7),
                    options: vec![option("yes", 0.8), option("no", 0.2)],
                },
                CouncilQuestion {
                    question_id: "verdict".into(),
                    mass: -0.5,
                    confidence: None,
                    options: vec![option("safe", 0.6), option("risky", 0.4)],
                },
            ],
            described: vec![CouncilDescribed { question_id: "why".into(), body: format!("because {tag}") }],
        }
    }

    fn decision() -> NewCouncilDecision {
        NewCouncilDecision {
            request_id: None,
            context_id: ASKING_CONTEXT.to_vec(),
            principal_id: vec![9, 9, 9],
            submission_digest: "digest-1".into(),
            spec_id: "shell-v1".into(),
            spec_name: "shell gate".into(),
            server: CouncilServer {
                model: "lfm2d".into(),
                weight_hash: "w1".into(),
                tokenizer_hash: "t1".into(),
                template: "chatml".into(),
                engine: "llama.cpp b1".into(),
            },
            pool_method: "loglinear".into(),
            pool_weights: "mass".into(),
            threshold: Some(CouncilThreshold { allow_at: 0.9, mass_floor: -1.5, require_agree: true }),
            deadline_ms: 400,
            outcome: CouncilOutcome::Allow,
            miss_cause: None,
            agreement: Some(CouncilAgreement { agree: true, spread: 0.125 }),
            queue_ms: 3,
            ms: 41,
            house_rules_head: Some("snap:rules".into()),
            bump_flavor: None,
            reads: vec![read(Some(&[1, 1]), "a"), read(None, "b")],
            pooled: vec![
                CouncilPooled { question_id: "undo".into(), option: "yes".into(), probability: 0.8 },
                CouncilPooled { question_id: "undo".into(), option: "no".into(), probability: 0.2 },
            ],
            control_text: vec![
                CouncilControlText { location: "state".into(), token: "<|im_end|>".into() },
                CouncilControlText { location: "context:abc:turn:2".into(), token: "<|im_start|>".into() },
            ],
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    const CHILD_TABLES: [&str; 6] = [
        "council_reads",
        "council_read_questions",
        "council_read_options",
        "council_pooled",
        "council_described",
        "council_control_text",
    ];

    #[test]
    fn a_decision_round_trips_with_two_reads_two_questions_and_options() {
        let conn = open_memory();
        let ask = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let mut input = decision();
        input.request_id = Some(ask.clone());

        let id = insert_council_decision(&conn, &input).unwrap();
        assert_eq!(id.len(), 16);

        let loaded = load_council_decision(&conn, &id).unwrap().expect("decision present");
        assert_eq!(loaded.decision_id, id);
        assert_eq!(loaded.control_text_hits, 2);
        assert!(loaded.created_at > 0);
        assert_eq!(loaded.decision, input, "every field and child must survive the round trip");
        assert_eq!(loaded.decision.reads[0].questions[0].options.len(), 2);
        assert_eq!(count(&conn, "council_read_options"), 8, "2 reads x 2 questions x 2 options");
        assert!(load_council_decision(&conn, &[0; 16]).unwrap().is_none());
    }

    #[test]
    fn a_decision_without_a_threshold_or_agreement_round_trips() {
        let conn = open_memory();
        let mut input = decision();
        input.threshold = None;
        input.agreement = None;
        input.house_rules_head = None;
        input.reads.clear();
        input.pooled.clear();
        input.control_text.clear();
        let id = insert_council_decision(&conn, &input).unwrap();
        let loaded = load_council_decision(&conn, &id).unwrap().unwrap();
        assert_eq!(loaded.decision, input);
        assert_eq!(loaded.control_text_hits, 0);
    }

    #[test]
    fn a_failing_child_insert_rolls_the_whole_decision_back() {
        let conn = open_memory();
        let mut input = decision();
        // The second read repeats an option of its own question: the
        // primary key refuses it after the decision and read 0 are written.
        input.reads[1].questions[0].options.push(option("yes", 0.5));

        let err = insert_council_decision(&conn, &input).expect_err("duplicate option must fail");
        assert!(matches!(err, LedgerError::Db(_)), "got {err:?}");

        assert_eq!(count(&conn, "council_decisions"), 0);
        for table in CHILD_TABLES {
            assert_eq!(count(&conn, table), 0, "{table} must be rolled back");
        }
        // The connection is usable afterward: nothing is left open.
        input.reads[1].questions[0].options.pop();
        insert_council_decision(&conn, &input).unwrap();
    }

    /// The gate records a decision in the transaction that creates its ask,
    /// so the two commit together or not at all.
    #[test]
    fn a_decision_inside_the_ask_transaction_commits_and_rolls_back_with_it() {
        let conn = open_memory();
        let linked = crate::ask::create_ask_recorded(&conn, &minimal_ask(), |tx, request_id| {
            let mut input = decision();
            input.request_id = Some(request_id.to_string());
            insert_council_decision_within(tx, &input).map(|_| ())
        })
        .unwrap();
        let listed = list_council_decisions_for_request(&conn, &linked, 10).unwrap();
        assert_eq!(listed.len(), 1, "the decision commits with its ask");

        let asks_before = count(&conn, "approvals");
        let failed = crate::ask::create_ask_recorded(&conn, &minimal_ask(), |tx, request_id| {
            let mut input = decision();
            input.request_id = Some(request_id.to_string());
            input.reads[1].questions[0].options.push(option("yes", 0.5));
            insert_council_decision_within(tx, &input).map(|_| ())
        });
        assert!(failed.is_err(), "a failing decision fails the ask transaction");
        assert_eq!(count(&conn, "approvals"), asks_before, "the ask rolls back with its decision");
        assert_eq!(count(&conn, "council_decisions"), 1);
    }

    #[test]
    fn an_unknown_request_id_is_refused_without_foreign_key_enforcement() {
        let conn = open_memory();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let mut input = decision();
        input.request_id = Some("no-such-ask".into());
        let err = insert_council_decision(&conn, &input).expect_err("unknown ask must be refused");
        assert!(matches!(err, LedgerError::NotFound(ref id) if id == "no-such-ask"), "got {err:?}");
        assert_eq!(count(&conn, "council_decisions"), 0);
    }

    #[test]
    fn the_schema_holds_the_request_id_to_a_real_ask() {
        let conn = open_memory();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let sql = "INSERT INTO council_decisions (decision_id, request_id, context_id, principal_id, submission_digest,
            spec_id, spec_name, server_model, weight_hash, tokenizer_hash, template, engine, pool_method,
            pool_weights, deadline_ms, outcome, control_text_hits, queue_ms, ms)
            VALUES (X'01', 'no-such-ask', X'01', X'02', 'd', 's', 's', 'm', 'w', 't', 'c', 'e', 'p', 'w', 1, 'allow', 0, 0, 0)";
        assert!(conn.execute(sql, []).is_err());
    }

    #[test]
    fn a_miss_needs_a_cause_and_only_a_miss_has_one() {
        let conn = open_memory();

        let mut miss = decision();
        miss.outcome = CouncilOutcome::Miss;
        miss.miss_cause = None;
        assert!(matches!(insert_council_decision(&conn, &miss), Err(LedgerError::InvalidCouncilDecision(_))));
        miss.miss_cause = Some(String::new());
        assert!(matches!(insert_council_decision(&conn, &miss), Err(LedgerError::InvalidCouncilDecision(_))));

        let mut caused = decision();
        caused.miss_cause = Some("deadline".into());
        assert!(matches!(insert_council_decision(&conn, &caused), Err(LedgerError::InvalidCouncilDecision(_))));
        assert_eq!(count(&conn, "council_decisions"), 0);

        miss.miss_cause = Some("deadline".into());
        let id = insert_council_decision(&conn, &miss).unwrap();
        let loaded = load_council_decision(&conn, &id).unwrap().unwrap();
        assert_eq!(loaded.decision.miss_cause.as_deref(), Some("deadline"));
    }

    #[test]
    fn a_bump_carries_its_flavor_and_only_a_bump_has_one() {
        let conn = open_memory();
        let mut bump = decision();
        bump.outcome = CouncilOutcome::Bump;
        bump.bump_flavor = None;
        assert!(matches!(insert_council_decision(&conn, &bump), Err(LedgerError::InvalidCouncilDecision(_))));
        bump.bump_flavor = Some(String::new());
        assert!(matches!(insert_council_decision(&conn, &bump), Err(LedgerError::InvalidCouncilDecision(_))));

        let mut flavored = decision();
        flavored.bump_flavor = Some("try_harder".into());
        assert!(matches!(insert_council_decision(&conn, &flavored), Err(LedgerError::InvalidCouncilDecision(_))));
        assert_eq!(count(&conn, "council_decisions"), 0);

        bump.bump_flavor = Some("try_harder".into());
        let id = insert_council_decision(&conn, &bump).unwrap();
        let loaded = load_council_decision(&conn, &id).unwrap().unwrap();
        assert_eq!(loaded.decision.outcome, CouncilOutcome::Bump);
        assert_eq!(loaded.decision.bump_flavor.as_deref(), Some("try_harder"));
    }

    /// A refused bump is a bump decision no ask links to. The count and the
    /// history are per (context, submission digest), oldest first.
    #[test]
    fn bumps_are_counted_per_context_and_submission_without_asks() {
        let conn = open_memory();
        let bump = |context: &[u8], digest: &str, flavor: &str| {
            let mut d = decision();
            d.context_id = context.to_vec();
            d.submission_digest = digest.into();
            d.outcome = CouncilOutcome::Bump;
            d.bump_flavor = Some(flavor.into());
            insert_council_decision(&conn, &d).unwrap();
        };
        bump(ASKING_CONTEXT, "d1", "try_harder");
        bump(ASKING_CONTEXT, "d1", "do_less");
        bump(ASKING_CONTEXT, "d2", "try_harder");
        bump(&[7, 7], "d1", "try_harder");
        let mut allowed = decision();
        allowed.submission_digest = "d1".into();
        insert_council_decision(&conn, &allowed).unwrap();
        // A bump that opened an ask at the limit is an ask's decision, not a refused bump.
        let ask = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let mut asked = decision();
        asked.request_id = Some(ask);
        asked.outcome = CouncilOutcome::Bump;
        asked.bump_flavor = Some("do_less".into());
        insert_council_decision(&conn, &asked).unwrap();

        // A submission whose shell decision proceeded but whose program bumped
        // is one refused bump; so is one where both bumped.
        let program_bump = |digest: &str, shell_outcome: CouncilOutcome, shell_flavor: Option<&str>, flavor: &str| {
            let mut program = decision();
            program.submission_digest = "program-text".into();
            program.outcome = CouncilOutcome::Bump;
            program.bump_flavor = Some(flavor.into());
            let program_id = insert_council_decision(&conn, &program).unwrap();
            let mut shell = decision();
            shell.submission_digest = digest.into();
            shell.outcome = shell_outcome;
            shell.bump_flavor = shell_flavor.map(str::to_owned);
            let shell_id = insert_council_decision(&conn, &shell).unwrap();
            insert_council_programs_within(&conn, &shell_id, &[CouncilProgram {
                statement_idx: 0,
                command: "python3 x.py".into(),
                language: "python".into(),
                path: None,
                sha256: None,
                imports_not_shown: vec![],
                unread_cause: None,
                program_decision_id: Some(program_id),
            }])
            .unwrap();
        };
        program_bump("d3", CouncilOutcome::Allow, None, "originals=changes");
        program_bump("d3", CouncilOutcome::Bump, Some("do_less"), "network=other");
        assert_eq!(list_bump_flavors(&conn, ASKING_CONTEXT, "d3").unwrap(), ["originals=changes", "do_less"]);

        assert_eq!(list_bump_flavors(&conn, ASKING_CONTEXT, "d1").unwrap(), ["try_harder", "do_less"]);
        assert_eq!(list_bump_flavors(&conn, ASKING_CONTEXT, "d2").unwrap(), ["try_harder"]);
        assert_eq!(list_bump_flavors(&conn, &[7, 7], "d1").unwrap(), ["try_harder"]);
        assert!(list_bump_flavors(&conn, ASKING_CONTEXT, "nothing").unwrap().is_empty());
    }

    /// Submissions are shell decisions, newest first; a submission is
    /// bumped when no ask links it and it did not pass. Its flavor follows
    /// the gate's names, and the pooled `undo` argmax rides along.
    #[test]
    fn submissions_read_bumps_from_unlinked_shell_decisions() {
        let conn = open_memory();
        let insert = |outcome: CouncilOutcome, flavor: Option<&str>, linked: bool, at: i64| {
            let mut d = decision();
            d.outcome = outcome;
            d.bump_flavor = flavor.map(str::to_owned);
            d.miss_cause = (outcome == CouncilOutcome::Miss).then(|| "low mass".to_string());
            d.control_text = vec![];
            if linked {
                d.request_id = Some(crate::ask::create_ask(&conn, &minimal_ask()).unwrap());
            }
            let id = insert_council_decision(&conn, &d).unwrap();
            conn.execute("UPDATE council_decisions SET created_at = ?1 WHERE decision_id = ?2", params![at, id]).unwrap();
            id
        };
        insert(CouncilOutcome::Allow, None, true, 100);
        insert(CouncilOutcome::Bump, Some("try_harder"), false, 200);
        insert(CouncilOutcome::Miss, None, false, 300);
        insert(CouncilOutcome::Bump, Some("do_less"), true, 400);
        insert(CouncilOutcome::Bump, Some("do_less"), false, 500);
        let all = list_council_submissions(&conn, &SubmissionFilter { limit: 10, ..Default::default() }).unwrap();
        let seen: Vec<(i64, bool, Option<&str>)> = all.iter().map(|s| (s.created_at, s.bumped, s.flavor.as_deref())).collect();
        assert_eq!(seen, [
            (500, true, Some("do_less")),
            (400, false, None),
            (300, true, Some("unjudged")),
            (200, true, Some("try_harder")),
            (100, false, None),
        ], "an ask-linked bump (at the limit) is not a refused bump");
        assert_eq!(all[0].undo, Some(("yes".to_string(), 0.8)), "the fixture's pooled undo argmax");
        let since = list_council_submissions(&conn, &SubmissionFilter { since_ms: Some(300), limit: 10, ..Default::default() }).unwrap();
        assert_eq!(since.len(), 3);
        let other = list_council_submissions(&conn, &SubmissionFilter { context_id: Some(vec![7, 7]), limit: 10, ..Default::default() }).unwrap();
        assert!(other.is_empty());
    }

    /// A program's own decision is not a submission; a shell decision that
    /// passed with a program that bumped is a bumped submission named by
    /// the program's flavor, and an unread program bumps as `unread`.
    #[test]
    fn a_program_bump_is_its_submission_s_bump() {
        let conn = open_memory();
        let mut program = decision();
        program.outcome = CouncilOutcome::Bump;
        program.bump_flavor = Some("originals=changes".into());
        let program_id = insert_council_decision(&conn, &program).unwrap();
        let shell_id = insert_council_decision(&conn, &decision()).unwrap();
        let row = |program_decision_id: Option<Vec<u8>>, unread: Option<&str>| CouncilProgram {
            statement_idx: 0,
            command: "python3 x.py".into(),
            language: "python".into(),
            path: None,
            sha256: None,
            imports_not_shown: vec![],
            unread_cause: unread.map(str::to_owned),
            program_decision_id,
        };
        insert_council_programs_within(&conn, &shell_id, &[row(Some(program_id), None)]).unwrap();
        let unread_shell = insert_council_decision(&conn, &decision()).unwrap();
        insert_council_programs_within(&conn, &unread_shell, &[row(None, Some("missing"))]).unwrap();
        let all = list_council_submissions(&conn, &SubmissionFilter { limit: 10, ..Default::default() }).unwrap();
        assert_eq!(all.len(), 2, "the program's own decision is not listed: {all:?}");
        let flavor_of = |id: &[u8]| all.iter().find(|s| s.decision_id == id).unwrap().flavor.clone();
        assert_eq!(flavor_of(&shell_id).as_deref(), Some("originals=changes"));
        assert_eq!(flavor_of(&unread_shell).as_deref(), Some("unread"));
    }

    /// The streak is the trailing run of bumps, oldest flavor first, and a
    /// pass ends it.
    #[test]
    fn the_bump_streak_is_the_trailing_run_of_bumps() {
        let at = |created_at: i64, bumped: bool, flavor: &str| CouncilSubmission {
            decision_id: vec![created_at as u8],
            context_id: ASKING_CONTEXT.to_vec(),
            created_at,
            submission_digest: "d".into(),
            outcome: if bumped { CouncilOutcome::Bump } else { CouncilOutcome::Allow },
            bumped,
            flavor: bumped.then(|| flavor.to_string()),
            undo: None,
        };
        let newest_first = [at(50, true, "do_less"), at(40, true, "try_harder"), at(30, false, ""), at(20, true, "x")];
        assert_eq!(bump_streak(&newest_first), Some(BumpStreak {
            count: 2,
            first_at: 40,
            last_at: 50,
            flavors: vec!["try_harder".into(), "do_less".into()],
        }));
        assert_eq!(bump_streak(&newest_first[2..]), None, "the newest passed");
        assert_eq!(bump_streak(&[]), None);
    }

    #[test]
    fn the_schema_refuses_the_same_rules_without_the_insert_function() {
        let conn = open_memory();
        let base = |outcome: &str, cause: &str, extra: &str| {
            format!(
                "INSERT INTO council_decisions (decision_id, context_id, principal_id, submission_digest, spec_id,
                    spec_name, server_model, weight_hash, tokenizer_hash, template, engine, pool_method, pool_weights,
                    deadline_ms, outcome, miss_cause, control_text_hits, queue_ms, ms {extra})
                 VALUES (randomblob(16), X'01', X'02', 'd', 's', 's', 'm', 'w', 't', 'c', 'e', 'p', 'w', 1,
                    '{outcome}', {cause}, 0, 0, 0 {})",
                if extra.is_empty() { "" } else { ", 0.5" }
            )
        };
        assert!(conn.execute(&base("miss", "NULL", ""), []).is_err(), "miss without a cause");
        assert!(conn.execute(&base("allow", "'late'", ""), []).is_err(), "cause without a miss");
        assert!(conn.execute(&base("allow", "NULL", ", allow_at"), []).is_err(), "a partial threshold");
        assert!(conn.execute(&base("allow", "NULL", ", spread"), []).is_err(), "spread without agree");
        assert!(conn.execute(&base("miss", "'late'", ""), []).is_ok());
        assert!(conn.execute(&base("allow", "NULL", ""), []).is_ok());
    }

    #[test]
    fn a_stored_outcome_the_code_does_not_know_fails_the_read_loudly() {
        let conn = open_memory();
        let id = insert_council_decision(&conn, &decision()).unwrap();
        conn.execute("UPDATE council_decisions SET outcome = 'shrug'", []).unwrap();
        assert!(load_council_decision(&conn, &id).is_err());
    }

    #[test]
    fn lists_come_newest_first_and_respect_the_limit() {
        let conn = open_memory();
        let ask = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let other_ask = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();

        let mut ids = Vec::new();
        for (i, request) in [Some(&ask), Some(&ask), Some(&other_ask), Some(&ask)].into_iter().enumerate() {
            let mut d = decision();
            d.request_id = request.cloned();
            d.submission_digest = format!("digest-{i}");
            if i == 3 {
                d.context_id = vec![5, 5];
            }
            ids.push(insert_council_decision(&conn, &d).unwrap());
        }
        // Timestamps decide the order, not insertion: make the first the newest.
        conn.execute(
            "UPDATE council_decisions SET created_at = 1000 + CASE submission_digest
                WHEN 'digest-0' THEN 900 WHEN 'digest-1' THEN 300 WHEN 'digest-2' THEN 500 ELSE 100 END",
            [],
        )
        .unwrap();

        let digests = |list: Vec<CouncilDecision>| -> Vec<String> {
            list.into_iter().map(|d| d.decision.submission_digest).collect()
        };
        assert_eq!(
            digests(list_council_decisions_for_request(&conn, &ask, 10).unwrap()),
            ["digest-0", "digest-1", "digest-3"]
        );
        assert_eq!(digests(list_council_decisions_for_request(&conn, &ask, 2).unwrap()), ["digest-0", "digest-1"]);
        assert_eq!(
            digests(list_council_decisions_for_context(&conn, ASKING_CONTEXT, 10).unwrap()),
            ["digest-0", "digest-2", "digest-1"]
        );
        assert_eq!(digests(list_council_decisions_for_context(&conn, &[5, 5], 10).unwrap()), ["digest-3"]);
        assert!(list_council_decisions_for_request(&conn, "none", 10).unwrap().is_empty());

        let first = &list_council_decisions_for_request(&conn, &ask, 1).unwrap()[0];
        assert_eq!(first.decision_id, ids[0]);
        assert_eq!(first.decision.reads.len(), 2, "list readers load children too");
    }

    #[test]
    fn equal_timestamps_fall_back_to_the_later_id() {
        let conn = open_memory();
        let first = insert_council_decision(&conn, &decision()).unwrap();
        let second = insert_council_decision(&conn, &decision()).unwrap();
        conn.execute("UPDATE council_decisions SET created_at = 5", []).unwrap();
        let listed = list_council_decisions_for_context(&conn, ASKING_CONTEXT, 10).unwrap();
        assert_eq!(listed.iter().map(|d| d.decision_id.clone()).collect::<Vec<_>>(), [second, first]);
    }

    #[test]
    fn the_list_readers_use_their_indexes() {
        let conn = open_memory();
        let plan = |sql: &str| -> String {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("; ")
        };
        let by_request = plan(
            "SELECT decision_id FROM council_decisions WHERE request_id = 'x' ORDER BY created_at DESC, decision_id DESC",
        );
        assert!(by_request.contains("idx_council_decisions_request"), "{by_request}");
        let by_context = plan(
            "SELECT decision_id FROM council_decisions WHERE context_id = X'01' ORDER BY created_at DESC, decision_id DESC",
        );
        assert!(by_context.contains("idx_council_decisions_context"), "{by_context}");
    }

    #[test]
    fn a_submissions_programs_round_trip_under_its_shell_decision() {
        let conn = open_memory();
        let shell = insert_council_decision(&conn, &decision()).unwrap();
        let mut program = decision();
        program.spec_name = "program-gate".into();
        let judged = insert_council_decision(&conn, &program).unwrap();
        let programs = vec![
            CouncilProgram {
                statement_idx: 0,
                command: "python3 /tmp/fix.py".into(),
                language: "python".into(),
                path: Some("/tmp/fix.py".into()),
                sha256: Some("sha256:ab".into()),
                imports_not_shown: vec!["helper".into()],
                unread_cause: None,
                program_decision_id: Some(judged),
            },
            CouncilProgram {
                statement_idx: 1,
                command: "python3 $x".into(),
                language: "python".into(),
                path: None,
                sha256: None,
                imports_not_shown: vec![],
                unread_cause: Some("expands at run time".into()),
                program_decision_id: None,
            },
        ];
        insert_council_programs_within(&conn, &shell, &programs).unwrap();
        assert_eq!(list_council_programs(&conn, &shell).unwrap(), programs);
        let mut both = programs[1].clone();
        both.program_decision_id = Some(shell.clone());
        assert!(insert_council_programs_within(&conn, &shell, &[both]).is_err(), "a read program has no unread cause");
    }
}
