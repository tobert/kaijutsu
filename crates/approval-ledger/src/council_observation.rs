//! Durable record of council observations and voice skips
//! (`docs/council.md`, "Council contexts are kaijutsu contexts").
//!
//! An observation is one read of a model character's voice under its own
//! spec, made after the gate decided. It hangs off the decision it observed
//! and never changes that decision's rows. A skip records a character on the
//! reviewer chain that has no voice context.
//!
//! [`insert_council_observation`] writes an observation and its question and
//! option rows in one transaction; [`insert_council_voice_skips`] writes a
//! decision's skips in one transaction. Readers return rows in the order they
//! were written.

use rusqlite::{Connection, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use strum::EnumString;

use crate::council::{CouncilOption, CouncilQuestion, CouncilServer};
use crate::error::{LedgerError, Result};
use crate::types::parse_enum;

/// How an observation ended. A `Miss` carries its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CouncilObservationOutcome {
    /// The server answered and the answer verified; `choice` names the
    /// option the read put most weight on.
    Answered,
    Miss,
}

impl CouncilObservationOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Miss => "miss",
        }
    }
}

/// Everything `insert_council_observation` records. The observation id and
/// the timestamp are the ledger's to assign.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewCouncilObservation {
    /// The council decision observed.
    pub decision_id: Vec<u8>,
    /// The voice context's label, `council-<character>`.
    pub seat_label: String,
    /// The voice context's id; `None` when the label resolved to no live
    /// context by the time it was read.
    pub seat_context_id: Option<Vec<u8>>,
    pub spec_name: String,
    /// Empty when the spec was never prepared.
    pub spec_id: String,
    /// Empty strings when the server never reported an identity.
    pub server: CouncilServer,
    pub outcome: CouncilObservationOutcome,
    /// The read's top option; set exactly when the outcome is `Answered`.
    pub choice: Option<String>,
    pub miss_cause: Option<String>,
    /// The snapshot the server read, when it answered.
    pub snapshot: Option<String>,
    /// The head the kernel expected the voice context to be at.
    pub expected_head: Option<String>,
    pub queue_ms: i64,
    pub ms: i64,
    pub questions: Vec<CouncilQuestion>,
}

/// A stored observation with its questions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CouncilObservation {
    pub observation_id: Vec<u8>,
    pub created_at: i64,
    #[serde(flatten)]
    pub observation: NewCouncilObservation,
}

/// A character on the reviewer chain with no `council-<character>` context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouncilVoiceSkip {
    pub principal_id: Vec<u8>,
    pub character_name: String,
}

fn invalid(message: impl Into<String>) -> LedgerError {
    LedgerError::InvalidCouncilObservation(message.into())
}

fn check(o: &NewCouncilObservation) -> Result<()> {
    match (o.outcome, o.miss_cause.as_deref(), o.choice.as_deref()) {
        (CouncilObservationOutcome::Miss, None, _) => Err(invalid("a miss must carry its cause")),
        (CouncilObservationOutcome::Miss, Some(""), _) => Err(invalid("a miss cause must not be empty")),
        (CouncilObservationOutcome::Miss, Some(_), Some(_)) => Err(invalid("a miss has no choice")),
        (CouncilObservationOutcome::Miss, Some(_), None) => Ok(()),
        (CouncilObservationOutcome::Answered, Some(_), _) => Err(invalid("only a miss carries a cause")),
        (CouncilObservationOutcome::Answered, None, None) => Err(invalid("an answered observation names its choice")),
        (CouncilObservationOutcome::Answered, None, Some(_)) => Ok(()),
    }
}

fn require_decision(conn: &Connection, decision_id: &[u8]) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM council_decisions WHERE decision_id = ?1)",
        [decision_id],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(invalid(format!("no council decision {} to attach to", hex(decision_id))))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Record one observation and its question and option rows in a single
/// transaction and return its id (16 UUIDv7 bytes). A `decision_id` that
/// names no decision is refused, whether or not the connection enforces
/// foreign keys. Any failure rolls the whole observation back.
pub fn insert_council_observation(conn: &Connection, o: &NewCouncilObservation) -> Result<Vec<u8>> {
    check(o)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    require_decision(&tx, &o.decision_id)?;
    let observation_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    tx.execute(
        "INSERT INTO council_observations (
            observation_id, decision_id, seat_label, seat_context_id, spec_name, spec_id, server_model,
            weight_hash, tokenizer_hash, template, engine, outcome, choice, miss_cause, snapshot,
            expected_head, queue_ms, ms, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            observation_id,
            o.decision_id,
            o.seat_label,
            o.seat_context_id,
            o.spec_name,
            o.spec_id,
            o.server.model,
            o.server.weight_hash,
            o.server.tokenizer_hash,
            o.server.template,
            o.server.engine,
            o.outcome.as_str(),
            o.choice,
            o.miss_cause,
            o.snapshot,
            o.expected_head,
            o.queue_ms,
            o.ms,
            crate::time::now_millis(),
        ],
    )?;
    for q in &o.questions {
        tx.execute(
            "INSERT INTO council_observation_questions (observation_id, question_id, mass, confidence)
             VALUES (?1, ?2, ?3, ?4)",
            params![observation_id, q.question_id, q.mass, q.confidence],
        )?;
        for opt in &q.options {
            tx.execute(
                "INSERT INTO council_observation_options (observation_id, question_id, option, logprob, probability)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![observation_id, q.question_id, opt.option, opt.logprob, opt.probability],
            )?;
        }
    }
    tx.commit()?;
    Ok(observation_id)
}

const OBSERVATION_COLUMNS: &str = "observation_id, decision_id, seat_label, seat_context_id, spec_name, spec_id, \
    server_model, weight_hash, tokenizer_hash, template, engine, outcome, choice, miss_cause, snapshot, \
    expected_head, queue_ms, ms, created_at";

fn decode(row: &Row<'_>) -> rusqlite::Result<CouncilObservation> {
    let outcome_raw: String = row.get("outcome")?;
    let outcome = parse_enum::<CouncilObservationOutcome>("outcome", &outcome_raw).map_err(|e| match e {
        LedgerError::Db(db) => db,
        other => rusqlite::Error::InvalidColumnType(0, other.to_string(), rusqlite::types::Type::Text),
    })?;
    Ok(CouncilObservation {
        observation_id: row.get("observation_id")?,
        created_at: row.get("created_at")?,
        observation: NewCouncilObservation {
            decision_id: row.get("decision_id")?,
            seat_label: row.get("seat_label")?,
            seat_context_id: row.get("seat_context_id")?,
            spec_name: row.get("spec_name")?,
            spec_id: row.get("spec_id")?,
            server: CouncilServer {
                model: row.get("server_model")?,
                weight_hash: row.get("weight_hash")?,
                tokenizer_hash: row.get("tokenizer_hash")?,
                template: row.get("template")?,
                engine: row.get("engine")?,
            },
            outcome,
            choice: row.get("choice")?,
            miss_cause: row.get("miss_cause")?,
            snapshot: row.get("snapshot")?,
            expected_head: row.get("expected_head")?,
            queue_ms: row.get("queue_ms")?,
            ms: row.get("ms")?,
            questions: Vec::new(),
        },
    })
}

/// The observations of one decision, oldest first, with their questions.
pub fn list_council_observations_for_decision(conn: &Connection, decision_id: &[u8]) -> Result<Vec<CouncilObservation>> {
    let mut observations = conn
        .prepare(&format!(
            "SELECT {OBSERVATION_COLUMNS} FROM council_observations WHERE decision_id = ?1
             ORDER BY created_at, observation_id"
        ))?
        .query_map([decision_id], decode)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for o in &mut observations {
        let id = o.observation_id.as_slice();
        let mut questions = conn
            .prepare(
                "SELECT question_id, mass, confidence FROM council_observation_questions
                 WHERE observation_id = ?1 ORDER BY rowid",
            )?
            .query_map([id], |row| {
                Ok(CouncilQuestion { question_id: row.get(0)?, mass: row.get(1)?, confidence: row.get(2)?, options: Vec::new() })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut options = conn.prepare(
            "SELECT question_id, option, logprob, probability FROM council_observation_options
             WHERE observation_id = ?1 ORDER BY rowid",
        )?;
        let mut rows = options.query([id])?;
        while let Some(row) = rows.next()? {
            let question_id: String = row.get(0)?;
            let question = questions
                .iter_mut()
                .find(|q| q.question_id == question_id)
                .ok_or_else(|| invalid(format!("option row for missing question {question_id}")))?;
            question.options.push(CouncilOption { option: row.get(1)?, logprob: row.get(2)?, probability: row.get(3)? });
        }
        o.observation.questions = std::mem::take(&mut questions);
    }
    Ok(observations)
}

/// Record a decision's voice skips, in chain order, in a single transaction.
/// A `decision_id` that names no decision is refused.
pub fn insert_council_voice_skips(conn: &Connection, decision_id: &[u8], skips: &[CouncilVoiceSkip]) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    insert_council_voice_skips_within(&tx, decision_id, skips)?;
    tx.commit()?;
    Ok(())
}

/// [`insert_council_voice_skips`] for a caller that already holds a
/// transaction on `conn`, such as the one that records the decision.
/// Nothing commits here.
pub fn insert_council_voice_skips_within(conn: &Connection, decision_id: &[u8], skips: &[CouncilVoiceSkip]) -> Result<()> {
    require_decision(conn, decision_id)?;
    for (seq, skip) in skips.iter().enumerate() {
        conn.execute(
            "INSERT INTO council_voice_skips (decision_id, seq, principal_id, character_name) VALUES (?1, ?2, ?3, ?4)",
            params![decision_id, seq as i64, skip.principal_id, skip.character_name],
        )?;
    }
    Ok(())
}

/// A decision's voice skips, in chain order.
pub fn list_council_voice_skips(conn: &Connection, decision_id: &[u8]) -> Result<Vec<CouncilVoiceSkip>> {
    Ok(conn
        .prepare("SELECT principal_id, character_name FROM council_voice_skips WHERE decision_id = ?1 ORDER BY seq")?
        .query_map([decision_id], |row| Ok(CouncilVoiceSkip { principal_id: row.get(0)?, character_name: row.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::council::{CouncilOutcome, NewCouncilDecision, insert_council_decision, load_council_decision};
    use crate::fixtures::{ASKING_CONTEXT, open_memory};

    fn server() -> CouncilServer {
        CouncilServer {
            model: "mk".into(),
            weight_hash: "w1".into(),
            tokenizer_hash: "t1".into(),
            template: "mk-letters-1:x".into(),
            engine: "e1".into(),
        }
    }

    fn decision(conn: &Connection) -> Vec<u8> {
        insert_council_decision(
            conn,
            &NewCouncilDecision {
                request_id: None,
                context_id: ASKING_CONTEXT.to_vec(),
                principal_id: vec![9],
                submission_digest: "d".into(),
                spec_id: "sha256:a".into(),
                spec_name: "shell-gate".into(),
                server: server(),
                pool_method: "loglinear".into(),
                pool_weights: "mass".into(),
                threshold: None,
                deadline_ms: 700,
                outcome: CouncilOutcome::Ask,
                miss_cause: None,
                agreement: None,
                queue_ms: 0,
                ms: 1,
                reads: vec![],
                pooled: vec![],
                control_text: vec![],
            },
        )
        .unwrap()
    }

    fn answered(decision_id: &[u8]) -> NewCouncilObservation {
        NewCouncilObservation {
            decision_id: decision_id.to_vec(),
            seat_label: "council-banto".into(),
            seat_context_id: Some(vec![4, 4]),
            spec_name: "direction-check".into(),
            spec_id: "sha256:b".into(),
            server: server(),
            outcome: CouncilObservationOutcome::Answered,
            choice: Some("strays".into()),
            miss_cause: None,
            snapshot: Some("snap:1".into()),
            expected_head: Some("snap:1".into()),
            queue_ms: 2,
            ms: 30,
            questions: vec![CouncilQuestion {
                question_id: "follows".into(),
                mass: -0.02,
                confidence: Some(0.6),
                options: vec![
                    CouncilOption { option: "follows".into(), logprob: -1.5, probability: 0.22 },
                    CouncilOption { option: "strays".into(), logprob: -0.3, probability: 0.75 },
                    CouncilOption { option: "unclear".into(), logprob: -3.5, probability: 0.03 },
                ],
            }],
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn an_observation_round_trips_with_its_options_and_leaves_the_decision_alone() {
        let conn = open_memory();
        let d = decision(&conn);
        let before = load_council_decision(&conn, &d).unwrap().unwrap();
        let input = answered(&d);
        let id = insert_council_observation(&conn, &input).unwrap();
        assert_eq!(id.len(), 16);
        let listed = list_council_observations_for_decision(&conn, &d).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].observation_id, id);
        assert!(listed[0].created_at > 0);
        assert_eq!(listed[0].observation, input, "every field and child survives the round trip");
        assert_eq!(load_council_decision(&conn, &d).unwrap().unwrap(), before);
        assert!(list_council_observations_for_decision(&conn, &[0; 16]).unwrap().is_empty());
    }

    #[test]
    fn a_miss_round_trips_without_a_seat_or_identity() {
        let conn = open_memory();
        let d = decision(&conn);
        let mut miss = answered(&d);
        miss.outcome = CouncilObservationOutcome::Miss;
        miss.choice = None;
        miss.miss_cause = Some("council context \"council-banto\" names no live context".into());
        miss.seat_context_id = None;
        miss.spec_id = String::new();
        miss.server = CouncilServer {
            model: String::new(),
            weight_hash: String::new(),
            tokenizer_hash: String::new(),
            template: String::new(),
            engine: String::new(),
        };
        miss.snapshot = None;
        miss.expected_head = None;
        miss.questions.clear();
        insert_council_observation(&conn, &miss).unwrap();
        assert_eq!(list_council_observations_for_decision(&conn, &d).unwrap()[0].observation, miss);
    }

    #[test]
    fn the_outcome_shape_is_refused_in_rust_and_in_sql() {
        let conn = open_memory();
        let d = decision(&conn);
        let mut o = answered(&d);
        o.choice = None;
        assert!(matches!(insert_council_observation(&conn, &o), Err(LedgerError::InvalidCouncilObservation(_))));
        o.choice = Some("follows".into());
        o.miss_cause = Some("late".into());
        assert!(matches!(insert_council_observation(&conn, &o), Err(LedgerError::InvalidCouncilObservation(_))));
        o.outcome = CouncilObservationOutcome::Miss;
        assert!(matches!(insert_council_observation(&conn, &o), Err(LedgerError::InvalidCouncilObservation(_))));
        o.choice = None;
        o.miss_cause = None;
        assert!(matches!(insert_council_observation(&conn, &o), Err(LedgerError::InvalidCouncilObservation(_))));
        assert_eq!(count(&conn, "council_observations"), 0);

        let raw = |outcome: &str, choice: &str, cause: &str| {
            format!(
                "INSERT INTO council_observations (observation_id, decision_id, seat_label, spec_name, spec_id,
                    server_model, weight_hash, tokenizer_hash, template, engine, outcome, choice, miss_cause,
                    queue_ms, ms)
                 VALUES (randomblob(16), X'{}', 'council-banto', 'direction-check', '', '', '', '', '', '',
                    '{outcome}', {choice}, {cause}, 0, 0)",
                hex(&d)
            )
        };
        assert!(conn.execute(&raw("miss", "NULL", "NULL"), []).is_err(), "miss without a cause");
        assert!(conn.execute(&raw("miss", "'strays'", "'late'"), []).is_err(), "miss with a choice");
        assert!(conn.execute(&raw("answered", "NULL", "NULL"), []).is_err(), "answer without a choice");
        assert!(conn.execute(&raw("answered", "'strays'", "'late'"), []).is_err(), "answer with a cause");
        assert!(conn.execute(&raw("miss", "NULL", "'late'"), []).is_ok());
        assert!(conn.execute(&raw("answered", "'strays'", "NULL"), []).is_ok());
    }

    #[test]
    fn an_unknown_decision_is_refused_without_foreign_key_enforcement() {
        let conn = open_memory();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        let err = insert_council_observation(&conn, &answered(&[7; 16])).expect_err("no such decision");
        assert!(matches!(err, LedgerError::InvalidCouncilObservation(ref m) if m.contains("0707")), "{err:?}");
        let skip = CouncilVoiceSkip { principal_id: vec![1], character_name: "lead".into() };
        assert!(insert_council_voice_skips(&conn, &[7; 16], &[skip]).is_err());
        assert_eq!(count(&conn, "council_observations") + count(&conn, "council_voice_skips"), 0);
    }

    #[test]
    fn a_failing_option_insert_rolls_the_whole_observation_back() {
        let conn = open_memory();
        let d = decision(&conn);
        let mut o = answered(&d);
        let repeat = o.questions[0].options[0].clone();
        o.questions[0].options.push(repeat);
        assert!(matches!(insert_council_observation(&conn, &o), Err(LedgerError::Db(_))));
        for table in ["council_observations", "council_observation_questions", "council_observation_options"] {
            assert_eq!(count(&conn, table), 0, "{table} must be rolled back");
        }
        o.questions[0].options.pop();
        insert_council_observation(&conn, &o).unwrap();
    }

    #[test]
    fn a_stored_outcome_the_code_does_not_know_fails_the_read_loudly() {
        let conn = open_memory();
        let d = decision(&conn);
        insert_council_observation(&conn, &answered(&d)).unwrap();
        conn.execute("UPDATE council_observations SET outcome = 'shrug'", []).unwrap();
        assert!(list_council_observations_for_decision(&conn, &d).is_err());
    }

    #[test]
    fn voice_skips_keep_chain_order() {
        let conn = open_memory();
        let d = decision(&conn);
        let skips = vec![
            CouncilVoiceSkip { principal_id: vec![2], character_name: "lead".into() },
            CouncilVoiceSkip { principal_id: vec![1], character_name: "banto".into() },
        ];
        insert_council_voice_skips(&conn, &d, &skips).unwrap();
        assert_eq!(list_council_voice_skips(&conn, &d).unwrap(), skips);
        assert!(list_council_voice_skips(&conn, &[0; 16]).unwrap().is_empty());
    }

    #[test]
    fn the_readers_use_their_indexes() {
        let conn = open_memory();
        let plan = |sql: &str| -> String {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            stmt.query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("; ")
        };
        let by_decision = plan(
            "SELECT observation_id FROM council_observations WHERE decision_id = X'01' ORDER BY created_at, observation_id",
        );
        assert!(by_decision.contains("idx_council_observations_decision"), "{by_decision}");
        let questions = plan("SELECT question_id FROM council_observation_questions WHERE observation_id = X'01'");
        assert!(questions.contains("USING PRIMARY KEY") || questions.contains("sqlite_autoindex"), "{questions}");
        let options = plan("SELECT option FROM council_observation_options WHERE observation_id = X'01'");
        assert!(options.contains("USING PRIMARY KEY") || options.contains("sqlite_autoindex"), "{options}");
        let skips = plan("SELECT character_name FROM council_voice_skips WHERE decision_id = X'01' ORDER BY seq");
        assert!(skips.contains("USING PRIMARY KEY") || skips.contains("sqlite_autoindex"), "{skips}");
    }
}
