//! The kernel's live roster, client side — "who is around right now" for
//! both humans and agents (`kaijutsu-kernel/src/roster.rs` is the design
//! record; `vfs::backends::roster` is the surface we read).
//!
//! The kernel publishes `/run/roster/index` as a generation-stamped TSV: one
//! header line, then one row per entity. We poll it, parse it, and hold the
//! result in [`RosterFeed`] for `ui::quick_context` (the panel) and
//! `ui::dock` (the presence count).
//!
//! **"Unknown, never absent" travels all the way to the pixels.** The kernel
//! stores liveness as a *computed* column and refuses to invent one; this
//! module refuses to flatten that away. Three consequences shape every type
//! here:
//!
//! - `Option<T>` really means "the kernel does not know", never "false" and
//!   never "skip the row".
//! - [`LivenessKind`] and [`Availability`] each carry an `Unknown(String)`
//!   arm. The kernel's vocabularies are deliberately open (a fourth liveness
//!   kind is anticipated in its own module doc); a value we have never heard
//!   of is preserved and displayed, not dropped and not a panic.
//! - A structurally malformed line — wrong column count, an unparseable
//!   `live`/`recorded_at` cell — is **counted** ([`ParsedIndex::unparsed`])
//!   and surfaced in the panel footer. Silently dropping it would turn a
//!   kernel bug into a shorter, plausible-looking list.
//!
//! [`RosterFetch`] is the same rule applied to the fetch itself: an empty
//! list must never be the rendering of "we never asked", "this kernel has no
//! roster", and "the read failed" all at once.
//!
//! ## Why the whole file, every poll
//!
//! The kernel stamps `FileAttr::generation` on this file precisely so a
//! caching reader can skip the read — but the **wire** `FileAttr`
//! (`kaijutsu.capnp`) has no `generation` field, so `Vfs.getattr` cannot
//! carry it to a client today, and `Vfs.snapshot` reports generation `0` for
//! any non-directory (`MountTable::snapshot_node`). A getattr-then-maybe-read
//! poll would therefore be two round trips that cannot actually skip the
//! second one. Until that wire gap closes (docs/issues.md) we read the whole
//! index — it is a handful of short lines — and re-parse only when the bytes
//! changed, which is where the real work is anyway. [`RosterFeed::revision`]
//! is a LOCAL content revision, not the kernel's generation; nothing here
//! pretends otherwise.
//!
//! The poll/drain shape (clone the handle, spawn an `IoTaskPool` task, ship
//! the result back through `RpcResultChannel`, drain it in a chained system)
//! is `connection::peers`' exactly.

use bevy::prelude::*;

use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};

/// The kernel's roster index — the filtered view (rows the kernel considers
/// "around"). `index-all` exists beside it for the unfiltered set; the panel
/// wants the same "who is around" answer the kernel itself would give.
pub const ROSTER_INDEX_PATH: &str = "/run/roster/index";

/// The exact header line `RosterFs::index_bytes` writes. A document whose
/// first line is not this is not a roster index we understand, and is
/// reported as an error rather than parsed optimistically — a header drift
/// that silently produced zero rows would look identical to an empty fleet.
const EXPECTED_HEADER: &str =
    "entity_kind\tentity_id\tlabel\tliveness_kind\tlive\thost\tstatus_text\tavailability\trecorded_at";

/// Column count of a roster row — the header's field count, checked per row.
const ROSTER_COLUMNS: usize = 9;

/// How often to re-read the index (seconds). Same cadence as
/// `peers::PEER_POLL_INTERVAL` and `drift::DRIFT_POLL_INTERVAL`: presence
/// has no push event on the wire, and "somebody appeared" is not a
/// sub-second concern.
pub const ROSTER_POLL_INTERVAL: f64 = 5.0;

// ============================================================================
// Row types
// ============================================================================

/// How the kernel knows whether this entity is live. Mirrors the kernel's
/// `LivenessKind` plus an [`LivenessKind::Unknown`] arm: the kernel's own
/// module doc anticipates a fourth kind, and a client that panicked (or
/// silently dropped a row) on first sight of one would make adding it a
/// breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessKind {
    /// A connection is open; death is *observed* on detach.
    Bound,
    /// Inferred from durable activity within a recency window.
    Recent,
    /// An external authoritative fact, re-verified each refresh.
    Attested,
    /// A kind this build has never heard of — preserved verbatim.
    Unknown(String),
}

impl LivenessKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "bound" => LivenessKind::Bound,
            "recent" => LivenessKind::Recent,
            "attested" => LivenessKind::Attested,
            other => LivenessKind::Unknown(other.to_string()),
        }
    }

    /// The row glyph: `●` observed, `◐` inferred, `◇` attested, `?` unknown.
    /// A kind we cannot name renders as an honest question mark rather than
    /// borrowing a confident glyph from a kind it might not be.
    pub fn glyph(&self) -> &'static str {
        match self {
            LivenessKind::Bound => "\u{25cf}",
            LivenessKind::Recent => "\u{25d0}",
            LivenessKind::Attested => "\u{25c7}",
            LivenessKind::Unknown(_) => "?",
        }
    }

    /// Group order in the panel: confident first, inferred, attested, then
    /// whatever we could not classify.
    pub fn group_order(&self) -> u8 {
        match self {
            LivenessKind::Bound => 0,
            LivenessKind::Recent => 1,
            LivenessKind::Attested => 2,
            LivenessKind::Unknown(_) => 3,
        }
    }
}

/// The entity's own report of whether a human is paying attention. Routing
/// data, never authorization (the kernel's rule, restated here so nothing in
/// the UI is tempted to gate on it). Open vocabulary, same as
/// [`LivenessKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Active,
    Idle,
    Away,
    Dnd,
    Unknown(String),
}

impl Availability {
    pub fn parse(s: &str) -> Self {
        match s {
            "active" => Availability::Active,
            "idle" => Availability::Idle,
            "away" => Availability::Away,
            "dnd" => Availability::Dnd,
            other => Availability::Unknown(other.to_string()),
        }
    }

    /// The short chip word shown beside a label.
    pub fn chip(&self) -> &str {
        match self {
            Availability::Active => "active",
            Availability::Idle => "idle",
            Availability::Away => "away",
            Availability::Dnd => "dnd",
            Availability::Unknown(s) => s,
        }
    }
}

/// One parsed roster row. Every `Option` is the kernel's own "unknown",
/// carried through untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    /// `principal` or `context` today; not an enum, because the kernel's
    /// entity vocabulary is its own to grow and we only ever display this.
    pub entity_kind: String,
    /// Hex entity id. Display-only here — joins happen kernel-side.
    pub entity_id: String,
    pub label: Option<String>,
    pub liveness_kind: Option<LivenessKind>,
    pub live: Option<bool>,
    pub host: Option<String>,
    pub status_text: Option<String>,
    pub availability: Option<Availability>,
    /// The KERNEL's clock, unix millis (`docs/midi.md`'s one-timebase rule:
    /// the kernel's stamp decides freshness, never the source's).
    pub recorded_at: Option<i64>,
}

impl RosterRow {
    /// What to call this row. A kernel that has no label for an entity is
    /// not a reason to show nothing: fall back to kind + a short id, the
    /// same shortening `ContextId::short` does, and say so by construction
    /// (`context:1a2b3c4d`).
    pub fn display_label(&self) -> String {
        match self.label.as_deref() {
            Some(l) if !l.is_empty() => l.to_string(),
            _ => {
                let short: String = self.entity_id.chars().take(8).collect();
                if short.is_empty() {
                    self.entity_kind.clone()
                } else {
                    format!("{}:{}", self.entity_kind, short)
                }
            }
        }
    }

    /// Sort key within a liveness group — by displayed name, so the panel
    /// does not reshuffle between polls that changed nothing visible.
    fn sort_key(&self) -> (String, String) {
        (self.display_label().to_lowercase(), self.entity_id.clone())
    }
}

/// The outcome of parsing one index document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedIndex {
    pub rows: Vec<RosterRow>,
    /// Lines that were not a row we could trust. Counted, never dropped
    /// quietly — the panel footer says how many.
    pub unparsed: usize,
}

// ============================================================================
// Parser (pure)
// ============================================================================

/// Parse a `/run/roster/index` document.
///
/// `Err` is reserved for "this is not a roster index" — an empty document or
/// an unrecognized header. Everything else is best-effort *per line* with a
/// count: one bad row must not cost us the rest of the fleet, and must not
/// vanish either.
///
/// Rows are returned grouped by liveness kind (bound, recent, attested,
/// unknown) and alphabetized within a group, which is the order the panel
/// renders; sorting here keeps the render a straight walk and makes the
/// ordering testable without Bevy.
pub fn parse_roster_index(text: &str) -> Result<ParsedIndex, String> {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Err("empty document (expected a roster index header)".to_string());
    };
    if header.trim_end_matches('\r') != EXPECTED_HEADER {
        return Err(format!("unrecognized roster index header: {header:?}"));
    }

    let mut out = ParsedIndex::default();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            // A blank line carries no row and no claim — nothing is lost by
            // stepping over it, unlike a wrong-arity line.
            continue;
        }
        match parse_row(line) {
            Some(row) => out.rows.push(row),
            None => out.unparsed += 1,
        }
    }

    out.rows.sort_by(|a, b| {
        let ka = a.liveness_kind.as_ref().map(|k| k.group_order()).unwrap_or(3);
        let kb = b.liveness_kind.as_ref().map(|k| k.group_order()).unwrap_or(3);
        ka.cmp(&kb).then_with(|| a.sort_key().cmp(&b.sort_key()))
    });
    Ok(out)
}

/// One TSV row → [`RosterRow`], or `None` if the line is not structurally a
/// row. An empty cell is the kernel's "unknown"; a *non-empty* cell we cannot
/// interpret (`live` that is not `true`/`false`, a `recorded_at` that is not
/// an integer) fails the whole line rather than being coerced — guessing
/// there would put a fabricated timestamp or liveness in front of a player.
fn parse_row(line: &str) -> Option<RosterRow> {
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() != ROSTER_COLUMNS {
        return None;
    }
    // The identity pair is the one part of a row that is NOT optional: the
    // kernel writes `kind_str()` and `to_hex()` unconditionally, and
    // `display_label`'s fallback is built from them. A row missing either is
    // structurally malformed, not an entity whose name we happen not to know
    // — rendering it would put a near-blank line in the panel that names
    // nobody and cannot be acted on. Counted like any other bad line.
    if cols[0].is_empty() || cols[1].is_empty() {
        return None;
    }

    let cell = |i: usize| -> Option<String> {
        let c = cols[i];
        if c.is_empty() {
            None
        } else {
            Some(c.to_string())
        }
    };

    let live = match cols[4] {
        "" => None,
        "true" => Some(true),
        "false" => Some(false),
        _ => return None,
    };
    let recorded_at = match cols[8] {
        "" => None,
        s => Some(s.parse::<i64>().ok()?),
    };

    Some(RosterRow {
        entity_kind: cols[0].to_string(),
        entity_id: cols[1].to_string(),
        label: cell(2),
        liveness_kind: cell(3).map(|s| LivenessKind::parse(&s)),
        live,
        host: cell(5),
        status_text: cell(6),
        availability: cell(7).map(|s| Availability::parse(&s)),
        recorded_at,
    })
}

// ============================================================================
// The feed resource
// ============================================================================

/// What we know about the last fetch. The panel renders each of these
/// distinctly; an empty row list on its own is never allowed to stand in for
/// any of them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RosterFetch {
    /// No poll has completed yet (no connection, or the first one is still
    /// in flight).
    #[default]
    Never,
    /// The last poll read and parsed the index.
    Fresh,
    /// The kernel answered "no such path" — an older kernel with no roster
    /// backend, or the mount is absent. Not an error to shout about, but not
    /// an empty fleet either.
    NoRoster { detail: String },
    /// The last poll failed, or returned something we could not parse.
    Error { detail: String },
}

/// The live roster as this client knows it.
#[derive(Resource, Default)]
pub struct RosterFeed {
    pub rows: Vec<RosterRow>,
    /// Malformed lines in the document the [`Self::rows`] came from.
    pub unparsed: usize,
    pub state: RosterFetch,
    /// LOCAL content revision — bumped when the index bytes actually changed.
    /// **Not** the kernel's `FileAttr::generation`, which does not reach
    /// clients today (see the module doc).
    pub revision: u64,
    /// `Time::elapsed_secs_f64()` of the last poll that produced rows, for
    /// the panel's staleness footer. `None` until the first success.
    pub last_success: Option<f64>,
    /// Last poll launch time, for the interval gate.
    last_poll: f64,
    /// The raw bytes behind [`Self::rows`], so an unchanged document costs
    /// no re-parse and no change-detection churn downstream.
    last_body: String,
}

impl RosterFeed {
    /// Whether the panel may show these rows as current data.
    pub fn is_fresh(&self) -> bool {
        matches!(self.state, RosterFetch::Fresh)
    }

    /// Fresh AND recent enough to still be worth asserting without a
    /// qualifier.
    ///
    /// [`Self::state`] alone is not enough. When the connection drops, the
    /// `RpcActor` resource goes away and [`poll_roster_index`] simply stops
    /// running — no error reply ever arrives, so the last successful fetch
    /// stays `Fresh` forever. Anything that cannot show its own staleness
    /// (the dock's one-glance badge) must ask this instead, and go silent
    /// when it answers `false`. The panel, which *does* print its age, is
    /// free to keep showing the last picture.
    pub fn is_current(&self, now: f64, max_age: f64) -> bool {
        self.is_fresh() && self.age_secs(now).is_some_and(|age| age <= max_age)
    }

    /// Seconds since the last successful read, or `None` if there has never
    /// been one.
    pub fn age_secs(&self, now: f64) -> Option<f64> {
        self.last_success.map(|t| (now - t).max(0.0))
    }

    /// `(bound, recent)` counts for the dock's presence summary. Rows the
    /// kernel has marked not-live are excluded from the count — "2 here"
    /// must mean two that are here.
    pub fn presence_counts(&self) -> (usize, usize) {
        let mut bound = 0;
        let mut recent = 0;
        for row in &self.rows {
            if row.live == Some(false) {
                continue;
            }
            match row.liveness_kind {
                Some(LivenessKind::Bound) => bound += 1,
                Some(LivenessKind::Recent) => recent += 1,
                _ => {}
            }
        }
        (bound, recent)
    }

    /// Install a parsed document, for tests of the consumers that read it.
    /// Production writes go through [`drain_roster_index`] and nowhere else.
    #[cfg(test)]
    pub fn set_for_test(&mut self, parsed: ParsedIndex, state: RosterFetch, now: f64) {
        self.rows = parsed.rows;
        self.unparsed = parsed.unparsed;
        self.state = state;
        self.last_success = Some(now);
        self.revision += 1;
    }
}

pub struct RosterFeedPlugin;

impl Plugin for RosterFeedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RosterFeed>()
            .add_systems(Update, (poll_roster_index, drain_roster_index).chain());
    }
}

/// Read `/run/roster/index` every [`ROSTER_POLL_INTERVAL`] —
/// `peers::poll_peer_roster`'s shape, one path instead of one RPC verb.
fn poll_roster_index(
    actor: Option<Res<RpcActor>>,
    mut feed: ResMut<RosterFeed>,
    time: Res<Time>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };

    let elapsed = time.elapsed_secs_f64();
    if elapsed - feed.last_poll < ROSTER_POLL_INTERVAL {
        return;
    }
    // Set immediately so concurrent requests cannot stack, same as the peer
    // and drift polls.
    feed.last_poll = elapsed;

    let handle = actor.handle.clone();
    let tx = result_channel.sender();

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            let result = match handle.vfs_read_all(ROSTER_INDEX_PATH).await {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(body) => Ok(body),
                    Err(e) => Err(format!("{ROSTER_INDEX_PATH}: not UTF-8: {e}")),
                },
                Err(e) => Err(format!("{e}")),
            };
            let _ = tx.send(RpcResultMessage::RosterIndexReceived { result });
        })
        .detach();
}

/// Whether a read failure means "this kernel has no roster" rather than
/// "something went wrong". Matched on the error text because that is all the
/// wire carries — `VfsError`'s variants collapse to a message string at the
/// capnp boundary. Deliberately narrow: anything unrecognized stays an
/// error, so a genuine fault is never dressed up as a missing feature.
fn reads_as_absent(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("not found") || d.contains("no mount point")
}

/// Drain a fetched index into [`RosterFeed`].
fn drain_roster_index(
    mut feed: ResMut<RosterFeed>,
    time: Res<Time>,
    mut events: MessageReader<RpcResultMessage>,
) {
    for event in events.read() {
        let RpcResultMessage::RosterIndexReceived { result } = event else {
            continue;
        };
        match result {
            Ok(body) => {
                if feed.is_fresh() && *body == feed.last_body {
                    // Same bytes: refresh only the staleness clock. No
                    // re-parse and no revision bump — but this IS a write,
                    // so `RosterFeed` still shows as changed and readers
                    // gated on `is_changed()` will re-run. That is wanted:
                    // the age they display just moved.
                    feed.last_success = Some(time.elapsed_secs_f64());
                    continue;
                }
                match parse_roster_index(body) {
                    Ok(parsed) => {
                        if parsed.unparsed > 0 {
                            warn!(
                                "roster: {} unparsable row(s) in {ROSTER_INDEX_PATH}",
                                parsed.unparsed
                            );
                        }
                        feed.rows = parsed.rows;
                        feed.unparsed = parsed.unparsed;
                        feed.state = RosterFetch::Fresh;
                        feed.last_body = body.clone();
                        feed.last_success = Some(time.elapsed_secs_f64());
                        feed.revision = feed.revision.wrapping_add(1);
                    }
                    Err(detail) => {
                        warn!("roster: {detail}");
                        feed.state = RosterFetch::Error { detail };
                        feed.last_body.clear();
                        feed.revision = feed.revision.wrapping_add(1);
                    }
                }
            }
            Err(detail) => {
                let state = if reads_as_absent(detail) {
                    RosterFetch::NoRoster {
                        detail: detail.clone(),
                    }
                } else {
                    warn!("roster: read failed: {detail}");
                    RosterFetch::Error {
                        detail: detail.clone(),
                    }
                };
                if feed.state != state {
                    feed.state = state;
                    feed.revision = feed.revision.wrapping_add(1);
                }
                feed.last_body.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A golden document in exactly the shape `RosterFs::index_bytes` writes.
    fn golden() -> String {
        format!(
            "{EXPECTED_HEADER}\n\
             principal\tdeadbeefcafef00d1122334455667788\tamy\tbound\ttrue\tmoltar\tat the keys\tactive\t1700000000000\n\
             context\t0011223344556677\t\trecent\t\t\t\t\t1699999940000\n"
        )
    }

    #[test]
    fn golden_header_and_row_parse() {
        let parsed = parse_roster_index(&golden()).expect("golden document parses");
        assert_eq!(parsed.unparsed, 0);
        assert_eq!(parsed.rows.len(), 2);

        let amy = &parsed.rows[0];
        assert_eq!(amy.entity_kind, "principal");
        assert_eq!(amy.label.as_deref(), Some("amy"));
        assert_eq!(amy.liveness_kind, Some(LivenessKind::Bound));
        assert_eq!(amy.live, Some(true));
        assert_eq!(amy.host.as_deref(), Some("moltar"));
        assert_eq!(amy.status_text.as_deref(), Some("at the keys"));
        assert_eq!(amy.availability, Some(Availability::Active));
        assert_eq!(amy.recorded_at, Some(1_700_000_000_000));
    }

    /// An empty cell is the kernel's "unknown" and must survive as `None` —
    /// never as `false`, never as an omitted row.
    #[test]
    fn empty_cells_are_unknown_not_false() {
        let parsed = parse_roster_index(&golden()).unwrap();
        let ctx = &parsed.rows[1];
        assert_eq!(ctx.label, None);
        assert_eq!(ctx.live, None, "an empty live cell is unknown, not dead");
        assert_eq!(ctx.host, None);
        assert_eq!(ctx.status_text, None);
        assert_eq!(ctx.availability, None);
        assert_eq!(ctx.liveness_kind, Some(LivenessKind::Recent));
    }

    /// The kernel's vocabularies are open. A value from a newer kernel is
    /// preserved verbatim and displayed — not dropped, not a panic.
    #[test]
    fn unknown_vocabulary_values_are_preserved() {
        let doc = format!(
            "{EXPECTED_HEADER}\n\
             principal\tabc123\tnew\theard\ttrue\t\t\tbrb\t1\n"
        );
        let parsed = parse_roster_index(&doc).unwrap();
        assert_eq!(parsed.unparsed, 0);
        assert_eq!(
            parsed.rows[0].liveness_kind,
            Some(LivenessKind::Unknown("heard".into()))
        );
        assert_eq!(
            parsed.rows[0].availability,
            Some(Availability::Unknown("brb".into()))
        );
        assert_eq!(
            parsed.rows[0].liveness_kind.as_ref().unwrap().glyph(),
            "?",
            "a kind we cannot name must not borrow a confident glyph"
        );
    }

    /// A wrong-arity line is counted, not dropped: the shorter list would
    /// otherwise be indistinguishable from a smaller fleet.
    #[test]
    fn malformed_lines_are_counted_and_the_rest_survive() {
        let doc = format!(
            "{EXPECTED_HEADER}\n\
             principal\tabc\tamy\tbound\ttrue\th\ts\tactive\t1\n\
             this is not a row\n\
             principal\tdef\tbob\tbound\tmaybe\t\t\t\t\n"
        );
        let parsed = parse_roster_index(&doc).unwrap();
        assert_eq!(parsed.rows.len(), 1, "only the good row survives");
        assert_eq!(
            parsed.unparsed, 2,
            "wrong arity AND an uninterpretable live cell both count"
        );
    }

    /// A row with no entity kind or no entity id names nobody — it is a
    /// malformed line, not a nameless entity, and must be counted rather
    /// than rendered as a near-blank row.
    #[test]
    fn a_row_without_an_identity_is_malformed() {
        let doc = format!(
            "{EXPECTED_HEADER}\n\
             \tabc\tamy\tbound\ttrue\t\t\t\t\n\
             principal\t\tbob\tbound\ttrue\t\t\t\t\n\
             principal\tdef\tcaz\tbound\ttrue\t\t\t\t\n"
        );
        let parsed = parse_roster_index(&doc).unwrap();
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.rows[0].display_label(), "caz");
        assert_eq!(parsed.unparsed, 2);
    }

    /// A non-integer `recorded_at` fails its line rather than being coerced
    /// to `None` — a fabricated "unknown age" is a quieter lie than a count.
    #[test]
    fn an_unparseable_timestamp_fails_its_row() {
        let doc = format!(
            "{EXPECTED_HEADER}\nprincipal\tabc\tamy\tbound\ttrue\t\t\t\tsoon\n"
        );
        let parsed = parse_roster_index(&doc).unwrap();
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.unparsed, 1);
    }

    /// A document that is not a roster index is an error, never zero rows.
    #[test]
    fn a_bad_header_is_an_error_not_an_empty_fleet() {
        assert!(parse_roster_index("").is_err(), "empty document");
        assert!(parse_roster_index("entity\tid\n").is_err(), "wrong header");
        // A header-only document IS a valid empty roster.
        let parsed = parse_roster_index(&format!("{EXPECTED_HEADER}\n")).unwrap();
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.unparsed, 0);
    }

    #[test]
    fn rows_group_by_liveness_kind_then_name() {
        let doc = format!(
            "{EXPECTED_HEADER}\n\
             context\t1\tzed\tbound\ttrue\t\t\t\t\n\
             context\t2\tabe\tattested\ttrue\t\t\t\t\n\
             context\t3\tmid\trecent\ttrue\t\t\t\t\n\
             context\t4\tnew\tteleported\ttrue\t\t\t\t\n\
             context\t5\tarn\tbound\ttrue\t\t\t\t\n"
        );
        let parsed = parse_roster_index(&doc).unwrap();
        let names: Vec<String> = parsed.rows.iter().map(|r| r.display_label()).collect();
        assert_eq!(names, vec!["arn", "zed", "mid", "abe", "new"]);
    }

    /// A row with no label still names itself — kind plus a short id, never
    /// a blank line the player cannot act on.
    #[test]
    fn a_labelless_row_falls_back_to_kind_and_short_id() {
        let row = RosterRow {
            entity_kind: "context".into(),
            entity_id: "0123456789abcdef0123".into(),
            label: None,
            liveness_kind: None,
            live: None,
            host: None,
            status_text: None,
            availability: None,
            recorded_at: None,
        };
        assert_eq!(row.display_label(), "context:01234567");
    }

    /// The dock's summary counts only entities the kernel has not marked
    /// dead — and never counts an unknown-liveness row as absent.
    #[test]
    fn presence_counts_skip_the_explicitly_not_live() {
        let doc = format!(
            "{EXPECTED_HEADER}\n\
             principal\t1\ta\tbound\ttrue\t\t\t\t\n\
             principal\t2\tb\tbound\tfalse\t\t\t\t\n\
             principal\t3\tc\tbound\t\t\t\t\t\n\
             context\t4\td\trecent\ttrue\t\t\t\t\n\
             context\t5\te\tattested\ttrue\t\t\t\t\n"
        );
        let parsed = parse_roster_index(&doc).unwrap();
        let mut feed = RosterFeed::default();
        feed.rows = parsed.rows;
        assert_eq!(
            feed.presence_counts(),
            (2, 1),
            "unknown liveness counts as present; explicit false does not"
        );
    }

    /// Only a "there is nothing here" answer may read as no-roster. A real
    /// fault must never be dressed up as a missing feature.
    ///
    /// The wrapped forms matter more than the bare ones: what the drain
    /// actually sees is a `CallError`, whose `Rpc` arm formats as `"RPC
    /// error: {0}"` around the capnp message that itself wraps `VfsError`'s
    /// Display. Testing only the inner text would pass while the real thing
    /// fell through to `Error`.
    #[test]
    fn absence_is_recognised_narrowly() {
        // Bare `VfsError` Display.
        assert!(reads_as_absent("not found: /run/roster/index"));
        assert!(reads_as_absent("no mount point for path: /run/roster/index"));
        // The shape `CallError::Rpc` actually produces.
        assert!(reads_as_absent(
            "RPC error: remote exception: not found: /run/roster/index"
        ));
        assert!(reads_as_absent(
            "RPC error: no mount point for path: /run/roster/index"
        ));

        assert!(!reads_as_absent("permission denied: /run/roster/index"));
        assert!(!reads_as_absent("connection reset"));
        assert!(!reads_as_absent(
            "RPC error: permission denied: /run/roster/index"
        ));
        // A disconnect must NOT read as "this kernel has no roster".
        assert!(!reads_as_absent("not ready: connecting (attempt 3)"));
        assert!(!reads_as_absent("actor shut down"));
        assert!(!reads_as_absent("call timed out after 10s"));
    }

    /// A dropped connection stops the poll without ever delivering an
    /// error, so `Fresh` alone can go stale silently. `is_current` is the
    /// guard against that, and it must expire.
    #[test]
    fn freshness_expires_even_though_the_state_never_changes() {
        let mut feed = RosterFeed::default();
        feed.set_for_test(ParsedIndex::default(), RosterFetch::Fresh, 100.0);

        assert!(feed.is_current(105.0, 15.0), "just fetched");
        assert!(feed.is_current(115.0, 15.0), "on the boundary");
        assert!(
            !feed.is_current(116.0, 15.0),
            "still Fresh, but nobody has answered in three poll intervals"
        );
        assert!(feed.is_fresh(), "the state itself is untouched — that is the trap");
    }

    #[test]
    fn a_feed_that_never_fetched_is_never_current() {
        let feed = RosterFeed::default();
        assert!(!feed.is_current(0.0, 15.0));
    }

    #[test]
    fn the_plugin_registers_the_feed_and_starts_ignorant() {
        let mut app = App::new();
        app.add_plugins(RosterFeedPlugin);
        let feed = app.world().resource::<RosterFeed>();
        assert_eq!(feed.state, RosterFetch::Never);
        assert!(feed.rows.is_empty());
        assert_eq!(feed.age_secs(10.0), None, "never fetched has no age");
    }
}
