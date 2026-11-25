//! `kj context info`: a context's metadata, built from sections.
//!
//! A **section** is one function from a read-only [`InfoView`] of the context
//! to an optional [`Section`]: a title, human lines, and JSON fields. The
//! [`REGISTRY`] lists the sections in display order. The human text is every
//! section's lines in that order, and the JSON record is every section's
//! fields merged into one object. A section with nothing to say returns
//! `Ok(None)` and adds nothing to either.
//!
//! # Adding a section
//!
//! A character, a model, or a feature that wants to tell a newly started
//! agent something about its context writes one function and adds one line to
//! [`REGISTRY`]:
//!
//! ```ignore
//! fn my_feature(view: &InfoView<'_>) -> Result<Option<Section>, String> {
//!     let Some(facts) = view.db.my_feature_facts(view.row.context_id).map_err(|e| e.to_string())? else {
//!         return Ok(None);
//!     };
//!     Ok(Some(
//!         Section::new("My feature")
//!             .line(format!("Feature: {facts}"))
//!             .field("my_feature", serde_json::json!(facts)),
//!     ))
//! }
//! ```
//!
//! A section receives the view and nothing else: the database handle, the
//! kernel, the context's row, the caller, and facts gathered before the
//! database lock was taken (the reviewer, the cost, the gate configuration).
//! A section must not:
//!
//! - write: no database write, no block, no state change of any kind;
//! - block: no waiting, no network, no file or process access. A read that
//!   needs `await` is gathered into [`InfoView`] by `context_info` before the
//!   sections run;
//! - print without bound: cap any list (see [`LIST_CAP`]) and say how many it
//!   left out;
//! - print a secret. Env values are the one exception, shown as stored.
//!
//! A read that fails returns `Err`, which fails the whole command and names
//! the section. An empty answer is `Ok(None)`, never an error. Two sections
//! that write the same JSON key also fail the command: each key has one owner.

use kaijutsu_types::ContextId;

use crate::kernel::Kernel;
use crate::kernel_db::{ContextRow, KernelDb};

use super::KjCaller;

/// The most items any one section lists before it says how many it left out.
pub(super) const LIST_CAP: usize = 5;

/// What a section reads. Everything here is read-only; the facts that need
/// `await` or `BlockStore` access are gathered before locking `KernelDb` so
/// sections never invert the `BlockStore -> KernelDb` lock order.
pub(super) struct InfoView<'a> {
    pub db: &'a KernelDb,
    pub kernel: &'a Kernel,
    /// Document version from `BlockStore`, absent when not resident.
    pub version: Option<u64>,
    /// Last note timestamp (Unix ms) on the performer's handoff log.
    pub handoff_note_at: Result<Option<i64>, String>,
    pub row: &'a ContextRow,
    pub caller: &'a KjCaller,
    /// The long-running trace id, from the in-memory drift handle.
    pub trace_id: Option<[u8; 16]>,
    /// The reviewer who routes an ask this context raises, or why none does.
    pub review: &'a Result<crate::approval_identity::ResolvedContextReview, String>,
    /// What the context's turns cost; `None` when it has recorded no calls.
    pub cost: Option<&'a crate::pricing::ContextCost>,
    /// `gate.toml`, as the kernel's config mount holds it now.
    pub gate: &'a super::gate_policy::GateConfigLoad,
}

/// Gather `BlockStore` facts without holding the `KernelDb` mutex.
pub(super) fn read_block_facts(
    db: &parking_lot::Mutex<KernelDb>,
    blocks: &crate::block_store::SharedBlockStore,
    row: &ContextRow,
) -> (Option<u64>, Result<Option<i64>, String>) {
    let handoff_ctx = match row.performer {
        Some(performer) => match db.lock().get_character(performer) {
            Ok(Some(sheet)) => Ok(sheet.handoff_ctx),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("performer sheet: {e}")),
        },
        None => Ok(None),
    };
    let version = blocks.version(row.context_id).ok();
    let handoff_note_at = match handoff_ctx {
        Err(e) => Err(e),
        Ok(None) => Ok(None),
        Ok(Some(log)) => match blocks.last_block_id(log) {
            None => Ok(None),
            Some(last) => blocks
                .get_block_snapshot(log, &last)
                .map(|snap| snap.map(|b| b.created_at as i64))
                .map_err(|e| format!("handoff log: {e}")),
        },
    };
    (version, handoff_note_at)
}

/// One section's output.
pub(super) struct Section {
    pub title: &'static str,
    pub lines: Vec<String>,
    pub json: serde_json::Map<String, serde_json::Value>,
}

impl Section {
    pub(super) fn new(title: &'static str) -> Self {
        Self { title, lines: Vec::new(), json: serde_json::Map::new() }
    }

    /// Add a human line. A line that holds a newline is several lines.
    pub(super) fn line(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }

    /// Add a JSON field under `key`.
    pub(super) fn field(mut self, key: &str, value: serde_json::Value) -> Self {
        self.json.insert(key.to_string(), value);
        self
    }
}

/// A section: reads the view, and returns what to show, or `None` for
/// nothing to say.
pub(super) type SectionFn = fn(&InfoView<'_>) -> Result<Option<Section>, String>;

/// The sections in display order.
pub(super) const REGISTRY: &[SectionFn] = &[
    identity,
    model_and_slots,
    state,
    work,
    lineage,
    loadout,
    rc_and_track,
    environment,
    usage,
];

/// Run `sections` over `view`: the human text, and the merged JSON record.
pub(super) fn build(
    view: &InfoView<'_>,
    sections: &[SectionFn],
) -> Result<(String, serde_json::Value), String> {
    let mut lines: Vec<String> = Vec::new();
    let mut record = serde_json::Map::new();
    for section in sections {
        let Some(section) = section(view)? else { continue };
        lines.extend(section.lines);
        for (key, value) in section.json {
            if record.insert(key.clone(), value).is_some() {
                return Err(format!("section '{}' writes '{key}', which another section already wrote", section.title));
            }
        }
    }
    Ok((lines.join("\n"), serde_json::Value::Object(record)))
}

/// `label (short-id)` for a context.
fn named(db: &KernelDb, id: ContextId) -> String {
    db.context_name(id)
}

/// `"{n} more"` suffix when a list was cut at [`LIST_CAP`].
fn more(total: usize) -> String {
    if total > LIST_CAP { format!(", and {} more", total - LIST_CAP) } else { String::new() }
}

fn identity(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    let (row, db) = (view.row, view.db);
    let is_current = view.caller.context_id == Some(row.context_id);
    let label = row.label.as_deref().unwrap_or("(none)");
    let mut section = Section::new("Identity")
        .line(if is_current { format!("Context: {label} *") } else { format!("Context: {label}") })
        .line(format!("ID:      {} (full {})", row.context_id.short(), row.context_id.to_hex()));
    if let Some(trace) = view.trace_id {
        section = section.line(format!("Trace:   {}", super::format::hex32(trace)));
    }
    section = section
        .line(format!(
            "Created: {} by {}",
            super::format::format_timestamp(row.created_at),
            db.name_for(row.created_by)
        ))
        // The registering client's self-reported hostname, `-` when unknown.
        // Always shown: "unknown" is itself the answer.
        .line(format!("Host:    {}", row.origin_host.as_deref().unwrap_or("-")));
    Ok(Some(
        section
            .field("context_id", row.context_id.to_hex().into())
            .field("context_id_short", row.context_id.short().into())
            .field("label", serde_json::json!(row.label))
            .field("is_current", is_current.into())
            .field("trace_id", serde_json::json!(view.trace_id.map(super::format::hex32)))
            .field("created_at", row.created_at.into())
            .field("created_by", row.created_by.to_hex().into())
            .field("created_by_name", db.name_for(row.created_by).into())
            // Advisory; see `ContextRow::origin_host`.
            .field("origin_host", serde_json::json!(row.origin_host)),
    ))
}

/// The settings a slot row carries, as `name=value` words.
fn slot_settings(slot: &crate::kernel_db::ContextSlotRow) -> Vec<(&'static str, String)> {
    let mut settings = Vec::new();
    let mut put = |name: &'static str, value: Option<String>| {
        if let Some(value) = value {
            settings.push((name, value));
        }
    };
    put("max_tokens", slot.max_tokens.map(|v| v.to_string()));
    put("context_window", slot.context_window.map(|v| v.to_string()));
    put("temperature", slot.temperature.map(|v| v.to_string()));
    put("top_p", slot.top_p.map(|v| v.to_string()));
    put("effort", slot.effort.clone());
    put("thinking_budget", slot.thinking_budget.map(|v| v.to_string()));
    put("thinking_style", slot.thinking_style.clone());
    put("decide", slot.decide.clone());
    put("preview_kib", slot.preview_kib.map(|v| v.to_string()));
    put("verbosity", slot.verbosity.clone());
    put("reasoning_summary", slot.reasoning_summary.clone());
    put("replay_phase", slot.replay_phase.clone());
    settings
}

fn model_and_slots(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    use crate::model_resolution::ContextModelLookup;
    let (db, id) = (view.db, view.row.context_id);
    let bound = crate::model_resolution::context_model(db, id).map_err(|e| format!("model binding: {e}"))?;
    let (backend, model, absent) = match &bound {
        ContextModelLookup::Model(m) => (Some(m.backend.clone()), Some(m.model.clone()), None),
        ContextModelLookup::None(none) => (None, None, none.notice()),
    };
    let mut section = Section::new("Model and slots");
    let slots = db.list_context_slots(id).map_err(|e| format!("slots: {e}"))?;
    let mut slot_json = Vec::new();
    let mut other_lines = Vec::new();
    let mut model_settings = String::new();
    for slot in &slots {
        let backend_name = db
            .get_backend(slot.backend_id)
            .map_err(|e| format!("backend of slot {}: {e}", slot.role))?
            .map_or_else(|| format!("(missing backend {})", slot.backend_id), |b| b.name);
        let settings = slot_settings(slot);
        let words: String = settings.iter().map(|(k, v)| format!(" {k}={v}")).collect();
        slot_json.push(serde_json::json!({
            "role": slot.role,
            "backend": backend_name,
            "model": slot.model,
            "settings": settings.iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v.clone()))).collect::<serde_json::Map<_, _>>(),
        }));
        if slot.role == crate::model_resolution::MODEL {
            model_settings = words;
        } else {
            other_lines.push(format!("Slot:     {} {backend_name}/{}{words}", slot.role, slot.model));
        }
    }
    match (&backend, &model, &absent) {
        (Some(b), Some(m), _) => section = section.line(format!("Model:    {b}/{m}{model_settings}")),
        (_, _, Some(notice)) => section = section.line(format!("Model:    {notice}")),
        _ => section = section.line("Model:    (the client brings its own)"),
    }
    for line in other_lines {
        section = section.line(line);
    }

    // Who judges this context's gate asks, from gate.toml and the performer's
    // sections. A model judge needs a `gate` slot on a chat backend; the
    // council needs its own switch.
    let (judge, judge_error) = match view.gate {
        Err(e) => (None, Some(format!("gate.toml did not load: {e}"))),
        Ok(config) => match super::gate_policy::performer_of(db, Some(id)) {
            Err(e) => (None, Some(e)),
            Ok(performer) => match crate::council::gate::judge_summary(db, config, id, performer.as_ref()) {
                Ok(judge) => (judge, None),
                Err(e) => (None, Some(e)),
            },
        },
    };
    section = match (&judge, &judge_error) {
        (Some(judge), _) => section.line(format!("Gate:     asks are judged by {judge}")),
        (None, Some(error)) => section.line(format!("Gate:     who judges could not be determined: {error}")),
        (None, None) => section.line("Gate:     no model judge or council is on for this context"),
    };
    Ok(Some(
        section
            .field("resolved_backend", serde_json::json!(backend))
            .field("resolved_model", serde_json::json!(model))
            .field("model_absent", serde_json::json!(absent))
            .field("slots", slot_json.into())
            .field("gate_judge", serde_json::json!(judge))
            .field("gate_judge_error", serde_json::json!(judge_error)),
    ))
}

fn state(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    use approval_ledger::types::Origin;
    let (db, row) = (view.db, view.row);
    let id = row.context_id;
    let in_flight = view.kernel.turn_in_flight(id);
    let last_turn = db.list_turns(id, 1).map_err(|e| format!("turns: {e}"))?.into_iter().next();
    let last_stop = last_turn.as_ref().and_then(|t| t.stop_reason.clone());
    let held = db.last_turn_was_refused(id).map_err(|e| format!("refusal hold: {e}"))?;
    let conn = db.conn_for_ledger();
    let refusal_asks = approval_ledger::ask::list_unsettled_asks(conn, id.as_bytes(), Origin::Refusal)
        .map_err(|e| format!("refusal asks: {e}"))?;
    let pending: Vec<String> = approval_ledger::ask::list_unresolved_for_context(conn, id.as_bytes())
        .map_err(|e| format!("pending asks: {e}"))?
        .into_iter()
        .map(|ask| ask.request_id)
        .collect();

    let state_word = format!("{:?}", row.context_state).to_lowercase();
    let turn_word = if in_flight { "a turn is in flight" } else { "no turn in flight" };
    let stop_word = match (&last_turn, &last_stop) {
        (None, _) => "no turns yet".to_string(),
        (Some(_), Some(reason)) => format!("last turn ended '{reason}'"),
        (Some(_), None) => "last turn has no stop reason".to_string(),
    };
    let mut section = Section::new("State").line(format!("State:   {state_word}; {turn_word}; {stop_word}"));
    if held {
        section = match refusal_asks.first() {
            Some(ask) => section.line(format!(
                "HELD:    the provider refused the latest turn; answer ask '{ask}' with `kj ledger allow '{ask}'`"
            )),
            None => section.line("HELD:    the provider refused the latest turn, and no refusal ask is open"),
        };
    }
    if !pending.is_empty() {
        let shown: Vec<String> = pending.iter().take(LIST_CAP).map(|id| format!("'{id}'")).collect();
        section = section.line(format!("Asks:    {} pending: {}{}", pending.len(), shown.join(", "), more(pending.len())));
    }
    for (label, stamp) in [("Ring:    seated", row.promoted_at), ("Ring:    demoted", row.demoted_at), ("Paused:  since", row.paused_at)] {
        if let Some(ts) = stamp {
            section = section.line(format!("{label} {}", super::format::format_timestamp(ts)));
        }
    }
    Ok(Some(
        section
            .field("context_state", format!("{:?}", row.context_state).into())
            .field("turn_in_flight", in_flight.into())
            .field("last_stop_reason", serde_json::json!(last_stop))
            .field("held", held.into())
            .field("refusal_ask", serde_json::json!(refusal_asks.first()))
            .field("pending_asks", serde_json::json!(pending))
            .field("promoted_at", serde_json::json!(row.promoted_at))
            .field("demoted_at", serde_json::json!(row.demoted_at))
            .field("paused_at", serde_json::json!(row.paused_at)),
    ))
}

fn work(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    let (db, row) = (view.db, view.row);
    let id = row.context_id;
    let kohai = db.list_kohai(id).map_err(|e| format!("contexts that answer to this one: {e}"))?;
    let forked = db.fork_children(id).map_err(|e| format!("fork children: {e}"))?.len();
    let drift = db.edges_from(id).map_err(|e| format!("drift edges: {e}"))?.len()
        + db.edges_to(id).map_err(|e| format!("drift edges: {e}"))?.len();
    let handoff_at = handoff_note_at(view)?;

    let mut section = Section::new("Work");
    if !kohai.is_empty() {
        let running = kohai.iter().filter(|k| view.kernel.turn_in_flight(k.context_id)).count();
        section = section.line(format!("Children: {} answer to this context ({running} running)", kohai.len()));
        for k in kohai.iter().take(LIST_CAP) {
            let word = if view.kernel.turn_in_flight(k.context_id) { "running" } else { "idle" };
            section = section.line(format!("  {} {word}", named(db, k.context_id)));
        }
        if kohai.len() > LIST_CAP {
            section = section.line(format!("  and {} more", kohai.len() - LIST_CAP));
        }
    }
    if forked > 0 {
        section = section.line(format!("Forks:   {forked} forked from this context"));
    }
    if drift > 0 {
        section = section.line(format!("Drifts:  {drift}"));
    }
    if let Some(at) = handoff_at {
        section = section.line(format!("Handoff: last note {}", super::format::format_timestamp(at)));
    }
    let kohai_json: Vec<_> = kohai
        .iter()
        .take(50)
        .map(|k| serde_json::json!({
            "context": k.context_id.to_hex(),
            "label": k.label,
            "turn_in_flight": view.kernel.turn_in_flight(k.context_id),
        }))
        .collect();
    Ok(Some(
        section
            .field("children_count", forked.into())
            .field("drift_count", drift.into())
            .field("kohai", kohai_json.into())
            .field("handoff_note_at", serde_json::json!(handoff_at)),
    ))
}

/// When the performer's handoff log last got a note, in Unix milliseconds.
fn handoff_note_at(view: &InfoView<'_>) -> Result<Option<i64>, String> {
    view.handoff_note_at.clone()
}

/// `label (short-id), played by <character>`; the character is left out when
/// it is the label, so a name is said once.
fn context_and_character(db: &KernelDb, id: ContextId) -> Result<String, String> {
    let Some(row) = db.get_context(id).map_err(|e| e.to_string())? else {
        return Ok(format!("{} (no row)", id.short()));
    };
    let name = db.context_name(id);
    Ok(match row.performer.map(|p| db.name_for(p)) {
        Some(character) if row.label.as_deref() != Some(character.as_str()) => format!("{name}, played by {character}"),
        _ => name,
    })
}

/// How a rotated context's `create` scripts compare with its predecessor's.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RcChange {
    Unchanged,
    Changed(Vec<String>),
    /// One side has no `create` run record, so nothing was compared.
    NotCompared(String),
}

impl RcChange {
    fn status(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Changed(_) => "changed",
            Self::NotCompared(_) => "not_compared",
        }
    }
}

/// The scripts (path, sha256) of a context's newest `create` run, `None` when
/// it has recorded none.
fn latest_create_scripts(db: &KernelDb, id: ContextId) -> Result<Option<(approval_ledger::types::RcRunRow, Vec<(String, String)>)>, String> {
    let conn = db.conn_for_ledger();
    let filter = approval_ledger::rc_runs::RunListFilter {
        context_id: Some(id.as_bytes().to_vec()),
        verb: Some("create".to_string()),
        since_ms: None,
        limit: 1,
    };
    let (runs, _) = approval_ledger::rc_runs::list_runs_filtered(conn, &filter).map_err(|e| format!("rc runs: {e}"))?;
    let Some(run) = runs.into_iter().next() else { return Ok(None) };
    let scripts = approval_ledger::rc_runs::list_run_scripts(conn, &run.run_id)
        .map_err(|e| format!("rc run scripts: {e}"))?
        .into_iter()
        .map(|s| (s.path, s.body_sha256))
        .collect();
    Ok(Some((run, scripts)))
}

/// Compare the `create` scripts of `predecessor` and `successor`: the paths
/// whose body hash differs, or that only one of them ran.
pub(super) fn compare_create_scripts(db: &KernelDb, predecessor: ContextId, successor: ContextId) -> Result<RcChange, String> {
    let Some((_, before)) = latest_create_scripts(db, predecessor)? else {
        return Ok(RcChange::NotCompared("the predecessor has no create run record".to_string()));
    };
    let Some((_, after)) = latest_create_scripts(db, successor)? else {
        return Ok(RcChange::NotCompared("this context has no create run record".to_string()));
    };
    let mut changed: Vec<String> = Vec::new();
    for (path, sha) in &after {
        if before.iter().find(|(p, _)| p == path).map(|(_, s)| s) != Some(sha) {
            changed.push(path.clone());
        }
    }
    for (path, _) in &before {
        if !after.iter().any(|(p, _)| p == path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed.dedup();
    Ok(if changed.is_empty() { RcChange::Unchanged } else { RcChange::Changed(changed) })
}

fn lineage(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    let (db, row) = (view.db, view.row);
    let id = row.context_id;
    let mut section = Section::new("Lineage");
    let performer_name = row.performer.and_then(|p| db.get_character(p).ok().flatten()).map(|c| c.name);
    section = section.line(format!("Performer: {}", performer_name.as_deref().unwrap_or("none")));

    if let Some(parent) = row.forked_from {
        let kind = row.fork_kind.as_ref().map(|k| format!("{k:?}"));
        section = section.line(match kind {
            Some(kind) => format!("Fork:    {}, {kind}", named(db, parent)),
            None => format!("Fork:    {}", named(db, parent)),
        });
    }

    let (reviewer_id, reviewer_name, review_source, reviewer_error) = match view.review {
        Ok(review) => (
            Some(review.reviewer.principal_id.to_hex()),
            Some(review.reviewer.name.clone()),
            Some(review.source.as_str()),
            None,
        ),
        Err(error) => (None, None, None, Some(error.clone())),
    };
    let senpai = match row.senpai_ctx {
        Some(sid) => Some(db.get_context(sid).map_err(|e| format!("senpai lookup: {e}"))?.ok_or_else(|| format!("senpai lookup: senpai {} has no row", sid.short()))?),
        None => None,
    };
    if let Some(senpai) = &senpai {
        section = section.line(format!("Senpai: {}", context_and_character(db, senpai.context_id)?));
    }
    match (&reviewer_name, &review_source, &reviewer_error) {
        (Some(name), Some(source), _) => section = section.line(format!("Reviewer: {name} ({source})")),
        (_, _, Some(error)) => section = section.line(format!("Reviewer error: {error}")),
        _ => {}
    }
    let root = db.lineage_root(id).map(|root| (root, db.name_for(root))).map_err(|e| e.to_string());
    match &root {
        Ok((_, name)) => section = section.line(format!("Lineage root: {name}")),
        Err(reason) => section = section.line(format!("Lineage root: none ({reason})")),
    }
    let note = db
        .senpai_migration_report()
        .map_err(|e| format!("migration notes: {e}"))?
        .into_iter()
        .find(|(c, _, _)| *c == id)
        .map(|(_, _, note)| note);
    if let Some(note) = &note {
        section = section.line(format!("Migration note: {note}"));
    }
    let character_notes: Vec<(String, String)> = db
        .context_character_migration_report()
        .map_err(|e| format!("migration notes: {e}"))?
        .into_iter()
        .filter(|n| n.context_id == Some(id))
        .map(|n| (n.kind, n.note))
        .collect();
    for (kind, note) in &character_notes {
        section = section.line(format!("Character migration note ({kind}): {note}"));
    }

    // The context this one replaced, and what its rc scripts did differently.
    let rotated_from = db
        .get_context_env(id)
        .map_err(|e| format!("env: {e}"))?
        .into_iter()
        .find(|v| v.key == "ROTATED_FROM")
        .map(|v| v.value);
    let mut predecessor_json = serde_json::Value::Null;
    let mut rc_change_json = serde_json::Value::Null;
    if let Some(raw) = &rotated_from {
        match ContextId::parse(raw) {
            Ok(pid) => {
                section = section.line(format!("Predecessor: {} (full {})", named(db, pid), pid.to_hex()));
                let change = compare_create_scripts(db, pid, id)?;
                section = section.line(match &change {
                    RcChange::Unchanged => "Rc changes since the predecessor: unchanged".to_string(),
                    RcChange::Changed(paths) => {
                        let shown: Vec<&str> = paths.iter().take(LIST_CAP).map(String::as_str).collect();
                        format!("Rc changes since the predecessor: {}{}", shown.join(", "), more(paths.len()))
                    }
                    RcChange::NotCompared(why) => format!("Rc changes since the predecessor: not compared (no run record): {why}"),
                });
                predecessor_json = serde_json::json!({ "id": pid.to_hex(), "name": named(db, pid) });
                rc_change_json = serde_json::json!({
                    "status": change.status(),
                    "changed_paths": match &change { RcChange::Changed(p) => p.clone(), _ => Vec::new() },
                    "reason": match &change { RcChange::NotCompared(why) => Some(why.clone()), _ => None },
                });
            }
            Err(_) => {
                section = section.line(format!("Predecessor: '{raw}' (not a context id)"));
                predecessor_json = serde_json::json!({ "id": null, "name": raw });
            }
        }
    }

    let forked_kind = row.fork_kind.as_ref().map(|k| format!("{k:?}"));
    let senpai_json = senpai.as_ref().map(|s| super::context::context_json(db, s));
    Ok(Some(
        section
            .field("forked_from", serde_json::json!(row.forked_from.map(|f| f.to_hex())))
            .field("fork_kind", serde_json::json!(forked_kind))
            .field("performer", serde_json::json!(row.performer.map(|p| p.to_hex())))
            .field("performer_name", serde_json::json!(performer_name))
            .field("reviewer_id", serde_json::json!(reviewer_id))
            .field("reviewer_name", serde_json::json!(reviewer_name))
            .field("review_source", serde_json::json!(review_source))
            .field("reviewer_error", serde_json::json!(reviewer_error))
            .field("senpai", serde_json::json!(senpai_json))
            .field("lineage_root", serde_json::json!(root.as_ref().ok().map(|(id, name)| serde_json::json!({ "id": id.to_hex(), "name": name }))))
            .field("lineage_root_error", serde_json::json!(root.as_ref().err()))
            .field("migration_note", serde_json::json!(note))
            .field(
                "character_migration_notes",
                character_notes.iter().map(|(kind, note)| serde_json::json!({ "kind": kind, "note": note })).collect::<Vec<_>>().into(),
            )
            .field("predecessor", predecessor_json)
            .field("rc_changes", rc_change_json),
    ))
}

fn loadout(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    let binding = view.db.get_context_binding(view.row.context_id).map_err(|e| format!("loadout: {e}"))?;
    let Some(binding) = binding.filter(|b| !b.is_empty()) else {
        return Ok(Some(
            Section::new("Loadout")
                .line("Loadout: none (no tools, facades, or authorities)")
                .field("loadout", serde_json::Value::Null),
        ));
    };
    let list = |items: Vec<String>| if items.is_empty() { "none".to_string() } else { items.join(", ") };
    let authorities: Vec<String> = binding.authorities.iter().cloned().collect();
    let instances = if binding.all_instances { "all".to_string() } else { binding.allowed_instances.len().to_string() };
    let facades = if binding.all_facades { "all".to_string() } else { list(binding.allowed_facades.clone()) };
    Ok(Some(
        Section::new("Loadout")
            .line(format!(
                "Loadout: authorities {}; {instances} instances, {} tools; facades {facades}",
                list(authorities.clone()),
                binding.allowed_tools.len(),
            ))
            .field("loadout", serde_json::json!({
                "authorities": authorities,
                "all_instances": binding.all_instances,
                "instances": binding.allowed_instances.len(),
                "tools": binding.allowed_tools.len(),
                "all_facades": binding.all_facades,
                "facades": binding.allowed_facades,
                "binding_admin": binding.binding_admin,
            })),
    ))
}

fn rc_and_track(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    let (db, row) = (view.db, view.row);
    let id = row.context_id;
    let rc_dir = match row.performer {
        Some(p) => db.get_character(p).map_err(|e| format!("performer sheet: {e}"))?.and_then(|s| s.rc_dir).filter(|d| !d.is_empty()),
        None => None,
    };
    let run = latest_create_scripts(db, id)?;
    let attachments: Vec<_> = db
        .list_all_attachments()
        .map_err(|e| format!("track attachments: {e}"))?
        .into_iter()
        .filter(|a| a.context_id == id)
        .collect();
    if rc_dir.is_none() && run.is_none() && attachments.is_empty() {
        return Ok(None);
    }
    let mut section = Section::new("Rc and track");
    if rc_dir.is_some() || run.is_some() {
        let dir = rc_dir.as_deref().unwrap_or("none");
        section = section.line(match &run {
            Some((run, scripts)) => {
                let outcome = run.outcome.map_or_else(
                    || run.intended_outcome.map_or("still running".to_string(), |o| format!("settlement pending ({o})")),
                    |o| o.to_string(),
                );
                format!("Rc:      directory '{dir}'; last create run {outcome}, {} scripts", run.script_count.unwrap_or(scripts.len() as i64))
            }
            None => format!("Rc:      directory '{dir}'; no create run recorded"),
        });
    }
    for a in &attachments {
        let rotate = a.rotate_every_phrases.map_or("never".to_string(), |n| format!("every {n} phrases"));
        section = section.line(format!("Track:   '{}' wakes every {} beats, rotates {rotate}", a.track_id, a.wakeup_every));
    }
    Ok(Some(
        section
            .field("rc", serde_json::json!({
                "dir": rc_dir,
                "last_create_run": run.as_ref().map(|(run, scripts)| serde_json::json!({
                    "run_id": run.run_id,
                    "outcome": run.outcome.map(|o| o.to_string()),
                    "script_count": run.script_count,
                    "scripts": scripts.len(),
                })),
            }))
            .field("tracks", attachments.iter().map(|a| serde_json::json!({
                "track_id": a.track_id,
                "wakeup_every": a.wakeup_every,
                "rotate_every_phrases": a.rotate_every_phrases,
                "ooda_armed": a.ooda_armed,
            })).collect::<Vec<_>>().into()),
    ))
}

fn environment(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    let (db, row) = (view.db, view.row);
    let id = row.context_id;
    let shell = db.get_context_shell(id).ok().flatten();
    let env_vars = db.get_context_env(id).unwrap_or_default();
    // A read failure is an error, never "none": an empty list claims this
    // context reaches nothing.
    let egress = db.list_context_egress(id).map_err(|e| format!("egress: {e}"))?;
    let controls = db.list_context_controls(id).map_err(|e| format!("controls: {e}"))?;
    let workspace_paths = db.context_workspace_paths(id).ok().flatten();
    let workspace_label = row.workspace_id.and_then(|w| db.get_workspace(w).ok().flatten()).map(|w| w.label);
    // The document version a client hydrates against. Absent when the
    // document is not resident in this kernel.
    let version = view.version;

    let mut section = Section::new("Environment");
    if let Some(v) = version {
        section = section.line(format!("Version: {v}"));
    }
    if let Some(cwd) = shell.as_ref().and_then(|s| s.cwd.as_ref()) {
        section = section.line(format!("Cwd:     {cwd}"));
    }
    if !env_vars.is_empty() {
        section = section.line("Env:");
        for v in &env_vars {
            section = section.line(format!("  {}={}", v.key, v.value));
        }
    }
    // Always shown, so a reader knows this context reaches nothing rather
    // than wondering whether the field was left out.
    if egress.is_empty() {
        section = section.line("Egress:  none");
    } else {
        section = section.line("Egress:");
        for h in &egress {
            section = section.line(format!("  {h}"));
        }
    }
    if !controls.is_empty() {
        section = section.line("Control:");
        for (position, item) in controls.iter().enumerate() {
            section = section.line(format!("  {position}  {}", super::context_control::describe_item(item)));
        }
    }
    if let Some(paths) = workspace_paths.as_ref().filter(|p| !p.is_empty()) {
        section = section.line(format!("Workspace: {}", workspace_label.clone().unwrap_or_else(|| "?".into())));
        for p in paths {
            section = section.line(format!("  {}{}", p.path, if p.read_only { " (ro)" } else { "" }));
        }
    }
    Ok(Some(
        section
            .field("version", serde_json::json!(version))
            .field("workspace_id", serde_json::json!(row.workspace_id.map(|id| id.to_hex())))
            .field("workspace_label", serde_json::json!(workspace_label))
            .field("preset_id", serde_json::json!(row.preset_id.map(|id| id.to_hex())))
            .field("cwd", serde_json::json!(shell.as_ref().and_then(|s| s.cwd.clone())))
            .field(
                "env",
                env_vars.iter().map(|v| (v.key.clone(), serde_json::Value::String(v.value.clone()))).collect::<serde_json::Map<_, _>>().into(),
            )
            // `docs/egress.md`: always present; an empty array is the honest "none".
            .field("egress", serde_json::json!(egress))
            .field("control", super::context_control::items_json(&controls)),
    ))
}

fn usage(view: &InfoView<'_>) -> Result<Option<Section>, String> {
    use super::context::{describe_cost, describe_tool_uses, tool_uses_json};
    let (db, id) = (view.db, view.row.context_id);
    // A SNAPSHOT of the last completed call ("how full is this context
    // now"), not a running total. `None` for a context that has never
    // completed one: absence, never a fabricated zero.
    let usage = db.get_context_usage(id).ok().flatten();
    let totals = db.context_turn_totals(id).map_err(|e| format!("turn totals: {e}"))?;
    let tool_uses = db.context_tool_use_totals(id).map_err(|e| format!("tool uses: {e}"))?;

    // The window belongs to the bound model: usage recorded on another model
    // has no window here. Both fields are `null` together, and the one
    // percentage-math site is `context_used_pct`.
    let bound = crate::model_resolution::context_model(db, id).map_err(|e| format!("model binding: {e}"))?;
    let bound_window = match &bound {
        crate::model_resolution::ContextModelLookup::Model(m) => m.context_window.map(|w| (m.backend.clone(), m.model.clone(), w)),
        crate::model_resolution::ContextModelLookup::None(_) => None,
    };
    let (window, used_pct) = match &usage {
        Some(u) => {
            let window = bound_window.as_ref().filter(|(b, m, _)| *b == u.provider && *m == u.model).map(|(_, _, w)| *w);
            (window, crate::kernel_db::context_used_pct(u, window))
        }
        None => (None, None),
    };

    let mut section = Section::new("Usage");
    if let Some(u) = &usage {
        let tail = match (window, used_pct) {
            (Some(w), Some(pct)) => format!(" — {w} window, {pct:.1}% used"),
            _ => " — window unknown".to_string(),
        };
        section = section.line(format!(
            "Tokens:  {} in + {} out = {} (cache read {}, write {}; {}/{}){tail}",
            u.input_tokens,
            u.output_tokens,
            u.input_tokens + u.output_tokens,
            u.cache_read_tokens,
            u.cache_write_tokens,
            u.provider,
            u.model,
        ));
    }
    section = section.line(format!(
        "Turns:   {} ({} calls): {} in, {} out, cache read {}, write {}, reasoning {}",
        totals.turns,
        totals.llm_calls,
        totals.input_tokens,
        totals.output_tokens,
        totals.cache_read_tokens,
        totals.cache_write_tokens,
        totals.reasoning_tokens,
    ));
    if !tool_uses.is_empty() {
        section = section.line(format!("Tools:   {}", describe_tool_uses(&tool_uses)));
    }
    if let Some(cost) = view.cost {
        section = section.line(format!("Cost:    {}", describe_cost(cost)));
    }
    Ok(Some(
        section
            .field("usage", serde_json::json!(usage.as_ref().map(|u| serde_json::json!({
                "provider": u.provider,
                "model": u.model,
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.input_tokens + u.output_tokens,
                "cache_read_tokens": u.cache_read_tokens,
                "cache_write_tokens": u.cache_write_tokens,
                "reasoning_tokens": u.reasoning_tokens,
                "updated_at_ms": u.updated_at,
                "context_window": window,
                "context_used_pct": used_pct,
            }))))
            .field("tool_uses", tool_uses_json(&tool_uses))
            .field("turns", serde_json::json!({
                "turns": totals.turns,
                "llm_calls": totals.llm_calls,
                "input_tokens": totals.input_tokens,
                "output_tokens": totals.output_tokens,
                "cache_read_tokens": totals.cache_read_tokens,
                "cache_write_tokens": totals.cache_write_tokens,
                "reasoning_tokens": totals.reasoning_tokens,
            }))
            // `micros` is null when no call could be priced: an unknown cost
            // is not a zero one.
            .field("cost", serde_json::json!(view.cost.map(|c| serde_json::json!({
                "micros": (c.priced_calls > 0).then_some(c.micros),
                "priced_calls": c.priced_calls,
                "unpriced_calls": c.unpriced_calls,
            })))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{
        caller_with_context, register_context, register_context_with_model, register_context_with_models, register_rooted_context, test_dispatcher,
    };
    use crate::kj::{KjDispatcher, KjResult};
    use kaijutsu_types::PrincipalId;

    fn s(text: &str) -> String {
        text.to_string()
    }

    /// Run `build` over `context` with `sections`.
    async fn build_with(d: &KjDispatcher, context: ContextId, sections: &[SectionFn]) -> Result<(String, serde_json::Value), String> {
        let row = d.kernel_db().lock().get_context(context).unwrap().unwrap();
        let caller = caller_with_context(context);
        let review = Err("not resolved in this test".to_string());
        let gate = super::super::gate_policy::no_config();
        let (version, handoff_note_at) = read_block_facts(d.kernel_db(), d.block_store(), &row);
        let db = d.kernel_db().lock();
        let view = InfoView {
            db: &db,
            kernel: d.kernel(),
            version,
            handoff_note_at,
            row: &row,
            caller: &caller,
            trace_id: None,
            review: &review,
            cost: None,
            gate: &gate,
        };
        build(&view, sections)
    }

    async fn info(d: &KjDispatcher, context: ContextId) -> (String, serde_json::Value) {
        let result = d.dispatch(&[s("context"), s("info"), context.to_hex()], &caller_with_context(context)).await;
        let KjResult::Ok { data: Some(data), .. } = &result else { panic!("info carries a record: {result:?}") };
        (result.message().to_string(), data.clone())
    }

    fn fake_section(_: &InfoView<'_>) -> Result<Option<Section>, String> {
        Ok(Some(Section::new("Fake").line("Fake:    from an extra section").field("fake_key", serde_json::json!(7))))
    }

    fn silent_section(_: &InfoView<'_>) -> Result<Option<Section>, String> {
        Ok(None)
    }

    fn clashing_section(_: &InfoView<'_>) -> Result<Option<Section>, String> {
        Ok(Some(Section::new("Clash").field("label", serde_json::json!("again"))))
    }

    /// The extension point: one function and one registry entry put a section
    /// in the text and the JSON; a section with nothing to say adds nothing;
    /// two sections writing one key fail.
    #[tokio::test]
    async fn an_extra_section_appears_in_the_text_and_the_json() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("extend-me"), None, PrincipalId::new());

        let (plain_text, plain_json) = build_with(&d, ctx, REGISTRY).await.unwrap();
        assert!(!plain_text.contains("from an extra section"), "{plain_text}");
        assert!(plain_json.get("fake_key").is_none());

        let extended: Vec<SectionFn> = REGISTRY.iter().copied().chain([silent_section as SectionFn, fake_section]).collect();
        let (text, json) = build_with(&d, ctx, &extended).await.unwrap();
        assert!(text.ends_with("Fake:    from an extra section"), "the extra section comes last: {text}");
        assert_eq!(json["fake_key"], 7);
        assert_eq!(json["label"], "extend-me", "the registered sections' keys stay");

        let clashing: Vec<SectionFn> = REGISTRY.iter().copied().chain([clashing_section as SectionFn]).collect();
        let error = build_with(&d, ctx, &clashing).await.unwrap_err();
        assert!(error.contains("'Clash'") && error.contains("'label'"), "{error}");
    }

    #[tokio::test]
    async fn identity_gives_both_id_forms_and_who_created_it() {
        let d = test_dispatcher().await;
        let ctx = register_rooted_context(&d, Some("who-am-i"), PrincipalId::new());
        let (text, json) = info(&d, ctx).await;
        assert!(text.contains(&format!("ID:      {} (full {})", ctx.short(), ctx.to_hex())), "{text}");
        assert_eq!(json["context_id"], ctx.to_hex());
        assert_eq!(json["context_id_short"], ctx.short());
        assert!(text.contains("Created: just now by "), "{text}");
        assert!(json["created_by"].is_string() && json["created_at"].is_i64(), "{json}");
        assert!(text.contains("Host:    -"), "{text}");
    }

    /// One line per slot, with the settings the slot row carries, and the
    /// judge of the gate.
    #[tokio::test]
    async fn every_slot_shows_its_backend_model_and_settings() {
        let d = test_dispatcher().await;
        let who = PrincipalId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_character(&crate::kernel_db::CharacterRow {
                rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None, principal_id: who,
                name: "slotter".into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
            db.seed_test_slot_entries(who, &[("model", "mock", "mock-model"), ("distill", "mock", "mock-small"), ("gate", "mock", "mock-judge")]).unwrap();
            let backend = db.get_backend_by_name("mock").unwrap().unwrap();
            db.set_character_model(&crate::kernel_db::CharacterModelRow {
                principal_id: who, backend_id: backend.backend_id, model: "mock-model".into(),
                max_tokens: Some(4096), temperature: None, top_p: None, effort: Some("high".into()),
                thinking_budget: None, thinking_style: None, decide: None, preview_kib: Some(16),
                verbosity: Some("low".into()), reasoning_summary: Some("auto".into()), replay_phase: Some("on".into()),
            }).unwrap();
        }
        let ctx = register_context(&d, Some("slotted"), None, who);
        d.kernel_db().lock().assign_stand_in_character(ctx, who).unwrap();
        let (text, json) = info(&d, ctx).await;
        let model_line = text.lines().find(|l| l.starts_with("Model:")).unwrap();
        for setting in ["effort=high", "max_tokens=4096", "verbosity=low", "reasoning_summary=auto", "replay_phase=on", "preview_kib=16"] {
            assert!(model_line.contains(setting), "{model_line}");
        }
        assert!(model_line.contains("mock/mock-model"), "{model_line}");
        assert!(text.contains("Slot:     distill mock/mock-small"), "{text}");
        assert!(text.contains("Slot:     gate mock/mock-judge"), "{text}");
        let slots = json["slots"].as_array().unwrap();
        assert_eq!(slots.len(), 3, "{json}");
        let model = slots.iter().find(|s| s["role"] == "model").unwrap();
        assert_eq!(model["settings"]["effort"], "high");
        assert_eq!(json["resolved_model"], "mock-model", "the existing key keeps its meaning");
        // No guardian is on, and gate.toml is empty: nobody judges.
        assert!(text.contains("Gate:     no model judge or council is on"), "{text}");
        assert!(json["gate_judge"].is_null());
    }

    #[tokio::test]
    async fn a_context_held_by_a_refusal_names_the_ask_that_releases_it() {
        use kaijutsu_types::TurnId;
        let d = test_dispatcher().await;
        let ctx = register_rooted_context(&d, Some("held"), PrincipalId::new());
        let turn = {
            let db = d.kernel_db().lock();
            let performer = db.context_character(ctx).unwrap().unwrap().principal_id;
            let turn = crate::kernel_db::TurnStart {
                turn_id: TurnId::new(), context_id: ctx, performer, role: "model".into(),
                provider: "mock".into(), model: "mock-model".into(), started_at: 1_000,
            };
            db.begin_turn(&turn).unwrap();
            db.finish_turn(turn.turn_id, 2_000, "refused").unwrap();
            turn
        };
        let ask = d.kernel().open_refusal_ask(ctx, turn.turn_id).unwrap().expect("an ask opens");

        let (text, json) = info(&d, ctx).await;
        assert!(text.contains("last turn ended 'refused'"), "{text}");
        assert!(text.contains(&format!("HELD:    the provider refused the latest turn; answer ask '{ask}' with `kj ledger allow '{ask}'`")), "{text}");
        assert!(text.contains(&format!("1 pending: '{ask}'")), "{text}");
        assert_eq!(json["held"], true);
        assert_eq!(json["refusal_ask"], ask);
        assert_eq!(json["pending_asks"], serde_json::json!([ask]));
        assert_eq!(json["last_stop_reason"], "refused");
        assert_eq!(json["context_state"], "Live", "the existing key keeps its meaning");
    }

    #[tokio::test]
    async fn a_context_with_no_refusal_is_not_held_and_shows_a_turn_in_flight() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("calm"), None, PrincipalId::new());
        let (text, json) = info(&d, ctx).await;
        assert!(text.contains("State:   live; no turn in flight; no turns yet"), "{text}");
        assert!(!text.contains("HELD:") && !text.contains("Asks:"), "{text}");
        assert_eq!(json["held"], false);
        let _lease = d.kernel().turns().begin(ctx);
        let (text, json) = info(&d, ctx).await;
        assert!(text.contains("a turn is in flight"), "{text}");
        assert_eq!(json["turn_in_flight"], true);
    }

    /// The contexts that answer to this one, each running or idle.
    #[tokio::test]
    async fn work_lists_the_contexts_that_answer_to_this_one_with_their_state() {
        let d = test_dispatcher().await;
        let lead = register_context_with_model(&d, Some("lead"), None, PrincipalId::new(), &["model"]);
        let busy = register_context(&d, Some("busy-kid"), Some(lead), PrincipalId::new());
        let calm = register_context(&d, Some("calm-kid"), Some(lead), PrincipalId::new());
        {
            let db = d.kernel_db().lock();
            db.copy_context_character(lead, busy).unwrap();
            db.copy_context_character(lead, calm).unwrap();
        }
        let _lease = d.kernel().turns().begin(busy);
        let (text, json) = info(&d, lead).await;
        assert!(text.contains("Children: 2 answer to this context (1 running)"), "{text}");
        assert!(text.contains(&format!("  busy-kid ({}) running", busy.short())), "{text}");
        assert!(text.contains(&format!("  calm-kid ({}) idle", calm.short())), "{text}");
        let kohai = json["kohai"].as_array().unwrap();
        assert_eq!(kohai.len(), 2, "{json}");
        assert_eq!(kohai.iter().find(|k| k["label"] == "busy-kid").unwrap()["turn_in_flight"], true);
        assert_eq!(json["children_count"], 2, "the fork count keeps its key");
    }

    #[tokio::test]
    async fn work_reports_the_age_of_the_last_handoff_note() {
        let d = test_dispatcher().await;
        let caller_ctx = register_context_with_model(&d, Some("noter"), None, PrincipalId::new(), &["model"]);
        let (none_text, none_json) = info(&d, caller_ctx).await;
        assert!(!none_text.contains("Handoff:"), "{none_text}");
        assert!(none_json["handoff_note_at"].is_null());

        let performer = d.kernel_db().lock().context_character(caller_ctx).unwrap().unwrap().principal_id;
        let log = register_context(&d, Some("noter-handoff"), None, performer);
        d.kernel_db().lock().set_character_handoff_ctx(performer, Some(log)).unwrap();
        d.block_store().create_document(log, crate::DocumentKind::Conversation, None).unwrap();
        let note = kaijutsu_types::BlockSnapshot::text(
            kaijutsu_types::BlockId::new(log, performer, 0), None, kaijutsu_types::Role::User, "remember the migration",
        );
        d.block_store().insert_from_snapshot_as(log, note, None, Some(performer)).unwrap();
        let (text, json) = info(&d, caller_ctx).await;
        assert!(text.contains("Handoff: last note just now"), "{text}");
        assert!(json["handoff_note_at"].is_i64(), "{json}");
    }

    /// A fork says what it was forked from, once, and no empty parentheses;
    /// the senpai is one name, not the label and the same character.
    #[tokio::test]
    async fn lineage_names_the_fork_and_the_senpai_once() {
        let d = test_dispatcher().await;
        let amy = crate::kj::test_helpers::register_root_context(&d);
        let kid = register_rooted_context(&d, Some("lineage-kid"), PrincipalId::new());
        let (text, _) = info(&d, kid).await;
        let senpai = text.lines().find(|l| l.starts_with("Senpai:")).unwrap();
        assert_eq!(senpai, format!("Senpai: amy ({})", amy.short()), "the label is the character, said once: {text}");
        assert!(!text.contains("()"), "no empty parentheses: {text}");
        let fork = text.lines().find(|l| l.starts_with("Fork:")).unwrap();
        assert!(fork.starts_with(&format!("Fork:    amy ({})", amy.short())), "{fork}");
        assert!(!fork.ends_with("()"), "{fork}");
    }

    async fn rotated_pair(d: &KjDispatcher) -> (ContextId, ContextId) {
        let predecessor = register_context(d, Some("old-seat"), None, PrincipalId::new());
        let successor = register_context(d, Some("new-seat"), None, PrincipalId::new());
        d.kernel_db().lock().set_context_env(successor, "ROTATED_FROM", &predecessor.to_hex()).unwrap();
        (predecessor, successor)
    }

    fn record_create_run(d: &KjDispatcher, ctx: ContextId, scripts: &[(&str, &str)]) {
        let db = d.kernel_db().lock();
        let conn = db.conn_for_ledger();
        let run = approval_ledger::rc_runs::start_run(conn, ctx.as_bytes(), "coder", "create").unwrap();
        approval_ledger::rc_runs::set_run_script_count(conn, &run, scripts.len()).unwrap();
        for (path, body) in scripts {
            approval_ledger::rc_runs::begin_run_script(conn, &run, path, body, 1).unwrap();
        }
    }

    #[tokio::test]
    async fn a_rotated_context_names_its_predecessor_and_the_scripts_that_changed() {
        let d = test_dispatcher().await;
        let (predecessor, successor) = rotated_pair(&d).await;
        record_create_run(&d, predecessor, &[("/config/rc/coder/create/S00-a.kai", "one"), ("/config/rc/coder/create/S01-b.kai", "two")]);
        record_create_run(&d, successor, &[("/config/rc/coder/create/S00-a.kai", "one"), ("/config/rc/coder/create/S01-b.kai", "TWO")]);
        let (text, json) = info(&d, successor).await;
        assert!(text.contains(&format!("Predecessor: old-seat ({}) (full {})", predecessor.short(), predecessor.to_hex())), "{text}");
        assert!(text.contains("Rc changes since the predecessor: /config/rc/coder/create/S01-b.kai"), "{text}");
        assert!(!text.contains("S00-a.kai"), "an unchanged script is not listed: {text}");
        assert_eq!(json["predecessor"]["id"], predecessor.to_hex());
        assert_eq!(json["rc_changes"]["status"], "changed");
        assert_eq!(json["rc_changes"]["changed_paths"], serde_json::json!(["/config/rc/coder/create/S01-b.kai"]));
    }

    #[tokio::test]
    async fn identical_scripts_are_unchanged_not_unknown() {
        let d = test_dispatcher().await;
        let (predecessor, successor) = rotated_pair(&d).await;
        record_create_run(&d, predecessor, &[("/config/rc/coder/create/S00-a.kai", "one")]);
        record_create_run(&d, successor, &[("/config/rc/coder/create/S00-a.kai", "one")]);
        let (text, json) = info(&d, successor).await;
        assert!(text.contains("Rc changes since the predecessor: unchanged"), "{text}");
        assert_eq!(json["rc_changes"]["status"], "unchanged");
    }

    #[tokio::test]
    async fn a_missing_run_record_is_not_compared_rather_than_unchanged() {
        let d = test_dispatcher().await;
        let (predecessor, successor) = rotated_pair(&d).await;
        record_create_run(&d, successor, &[("/config/rc/coder/create/S00-a.kai", "one")]);
        let (text, json) = info(&d, successor).await;
        assert!(text.contains("Rc changes since the predecessor: not compared (no run record)"), "{text}");
        assert!(!text.contains("unchanged"), "{text}");
        assert_eq!(json["rc_changes"]["status"], "not_compared");
        let _ = predecessor;
    }

    #[tokio::test]
    async fn loadout_counts_what_the_context_holds() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("armed"), None, PrincipalId::new());
        d.kernel_db().lock().delete_context_binding(ctx).unwrap();
        let (bare, bare_json) = info(&d, ctx).await;
        assert!(bare.contains("Loadout: none"), "{bare}");
        assert!(bare_json["loadout"].is_null());

        let mut binding = crate::mcp::binding::ContextToolBinding::new();
        binding.authorities.insert("drive".to_string());
        binding.allowed_facades.push("shell".to_string());
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();
        let (text, json) = info(&d, ctx).await;
        assert!(text.contains("Loadout: authorities drive; 0 instances, 0 tools; facades shell"), "{text}");
        assert_eq!(json["loadout"]["authorities"], serde_json::json!(["drive"]));
    }

    #[tokio::test]
    async fn rc_and_track_show_the_create_run_and_the_beat_attachment() {
        let d = test_dispatcher().await;
        let ctx = register_context(&d, Some("on-beat"), None, PrincipalId::new());
        let (bare, bare_json) = info(&d, ctx).await;
        assert!(!bare.contains("Rc:") && !bare.contains("Track:"), "a section with nothing to say is left out: {bare}");
        assert!(bare_json.get("rc").is_none());

        record_create_run(&d, ctx, &[("/config/rc/coder/create/S00-a.kai", "one")]);
        {
            let db = d.kernel_db().lock();
            db.upsert_track(&crate::kernel_db::PersistedTrack {
                track_id: "groove".into(), period_ms: 500, beats_per_phrase: 4, playhead_tick: None,
                playing: false, score_context_id: None, clock_kind: "system".into(),
            }).unwrap();
            db.upsert_attachment(&crate::kernel_db::PersistedAttachment {
                track_id: "groove".into(), context_id: ctx, wakeup_every: 8, rotate_every_phrases: Some(16), ooda_armed: false,
            }).unwrap();
        }
        let (text, json) = info(&d, ctx).await;
        assert!(text.contains("Rc:      directory 'none'; last create run still running, 1 scripts"), "{text}");
        assert!(text.contains("Track:   'groove' wakes every 8 beats, rotates every 16 phrases"), "{text}");
        assert_eq!(json["tracks"][0]["wakeup_every"], 8);
    }
}
