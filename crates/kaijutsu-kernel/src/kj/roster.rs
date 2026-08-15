//! `kj roster` — the live roster (`crate::roster` module doc for the whole
//! design). `status` (slice 3) posts a self-reported status; `list` (slice 4)
//! reads the current view.
//!
//! Identity is stamped kernel-side from the connection ([`KjCaller`]), never
//! parsed from an argument — the design record's joinability rule (`kj
//! midi`/`peers.rs` follow the same stance: `PeerConfig::principal` is
//! stamped server-side, never client-supplied). A caller with an active
//! context posts as that context (an agent reporting its own status); a
//! caller with none posts as its principal (a human at the shell, not
//! attached to any context). There is no `--as`/`--principal`/`--context`
//! override anywhere in this file — accepting one would let a caller post a
//! status for an identity it doesn't hold.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::roster::{Availability, RosterEntity};

#[derive(Parser, Debug)]
#[command(
    name = "roster",
    about = "The live roster — who's around right now",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct RosterArgs {
    #[command(subcommand)]
    command: RosterCommand,
}

#[derive(Subcommand, Debug)]
enum RosterCommand {
    /// Post (or update) your own self-reported status. Identity is always
    /// the caller's own — there is no way to post for anyone else.
    Status {
        /// Status text. Quote it if it has spaces.
        text: String,
        /// active | idle | away | dnd. Omit to keep your current
        /// availability (defaults to `active` on your first post).
        #[arg(long)]
        availability: Option<String>,
    },
    /// List the current roster — who's around right now.
    #[command(alias = "ls")]
    List {
        /// Emit a JSON array of row objects instead of a labelled table.
        #[arg(long)]
        json: bool,
        /// Include entities we positively know are NOT live. Off by default:
        /// the `recent` source carries one row per non-archived context, so
        /// this is usually a few live entities among hundreds of idle ones.
        /// Applies to `--json` too.
        #[arg(long)]
        all: bool,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_roster(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<RosterArgs>();
        }
        let parsed = match RosterArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj roster: {e}"));
            }
        };
        match parsed.command {
            RosterCommand::Status { text, availability } => {
                self.roster_status(&text, availability.as_deref(), caller).await
            }
            RosterCommand::List { json, all } => self.roster_list(json, all).await,
        }
    }

    /// **Boot rule** (`crate::roster` module doc): a row persisted before
    /// this process's first refresh must never render as current. Rather
    /// than depend on `roster_sources::spawn_periodic_refresh` having been
    /// started somewhere (it isn't wired into production boot yet — see
    /// `roster_sources.rs`'s module doc), a read surface satisfies the rule
    /// itself: run one refresh pass inline the first time anything reads the
    /// roster, then rely on the periodic loop (once it exists) for ongoing
    /// freshness. Idempotent to call on every `kj roster list` — a no-op
    /// once `refreshed_at()` is set.
    async fn ensure_refreshed(&self) -> KjResult {
        if self.roster().refreshed_at().is_none() {
            let peers = self.kernel().list_peers().await;
            if let Err(e) =
                crate::roster_sources::refresh_once(self.kernel_db(), self.roster(), &peers)
            {
                return KjResult::Err(format!("kj roster: initial refresh failed: {e}"));
            }
        }
        KjResult::ok("")
    }

    /// Hides ONLY what we positively know is dead. `live == None` is
    /// *unknown* — a status-only entity's shape — and unknowns stay visible
    /// ("missing entry = unknown, never absent"). See `vfs::backends::roster`.
    fn is_around(row: &crate::roster::RosterRow) -> bool {
        row.live != Some(false)
    }

    async fn roster_list(&self, json: bool, all: bool) -> KjResult {
        if let KjResult::Err(e) = self.ensure_refreshed().await {
            return KjResult::Err(e);
        }
        let known = match self.roster().snapshot() {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj roster list: {e}")),
        };
        // Group by ENTITY, newest first, exactly as `/run/roster/index` does
        // (`vfs::backends::roster::rows_by_key`). `roster_presence` is keyed
        // `(source, source_local_id)` — one row per *connection* — so one
        // principal with two windows open is two rows, and rendering them raw
        // showed the same entity twice. The two surfaces must not disagree
        // about the same data; the connection count is kept rather than
        // dropped so grouping loses nothing a reader wanted.
        let total_entities = {
            let mut seen: std::collections::BTreeSet<String> = Default::default();
            for r in &known {
                seen.insert(entity_key(r));
            }
            seen.len()
        };
        let mut grouped: std::collections::BTreeMap<String, (crate::roster::RosterRow, usize)> =
            Default::default();
        for row in known.into_iter().filter(|r| all || Self::is_around(r)) {
            let key = entity_key(&row);
            match grouped.get_mut(&key) {
                Some((existing, n)) => {
                    *n += 1;
                    if row.recorded_at >= existing.recorded_at {
                        *existing = row;
                    }
                }
                None => {
                    grouped.insert(key, (row, 1));
                }
            }
        }
        let counts: Vec<usize> = grouped.values().map(|(_, n)| *n).collect();
        let rows: Vec<crate::roster::RosterRow> =
            grouped.into_values().map(|(r, _)| r).collect();
        let total = total_entities;
        let hidden = total.saturating_sub(rows.len());

        let data = serde_json::Value::Array(rows.iter().map(row_to_json).collect());
        if json {
            return KjResult::ok_with_data(data.to_string(), data);
        }
        if rows.is_empty() {
            let msg = if total == 0 {
                "(nobody on the roster yet)".to_string()
            } else {
                format!("(nobody around right now — {total} known, `--all` to list them)")
            };
            return KjResult::ok_with_data(msg, data);
        }

        let kind_w = rows.iter().map(|r| r.entity.kind_str().len()).max().unwrap_or(0);
        let label_w = rows
            .iter()
            .map(|r| r.label.as_deref().unwrap_or("").len())
            .max()
            .unwrap_or(0);
        let lines: Vec<String> = rows
            .iter()
            .map(|r| {
                let kind = r.entity.kind_str();
                let id = r.entity.short();
                let label = r.label.as_deref().unwrap_or("");
                let liveness = match (r.liveness_kind, r.live) {
                    (Some(k), Some(true)) => format!("{} live", k.as_str()),
                    (Some(k), Some(false)) => format!("{} idle", k.as_str()),
                    (Some(k), None) => k.as_str().to_string(),
                    (None, _) => "unknown".to_string(),
                };
                let status = match (&r.status_text, r.availability) {
                    (Some(t), Some(a)) => format!("  \"{t}\" ({})", a.as_str()),
                    (Some(t), None) => format!("  \"{t}\""),
                    (None, Some(a)) => format!("  ({})", a.as_str()),
                    (None, None) => String::new(),
                };
                format!("  {kind:<kind_w$} {id}  {label:<label_w$}  {liveness}{status}")
            })
            .collect();
        // `×N` where one entity holds N live connections (two app windows, two
        // MCP sessions). Absent for the ordinary single-connection case.
        let lines: Vec<String> = lines
            .into_iter()
            .zip(counts.iter())
            .map(|(line, n)| if *n > 1 { format!("{line}  ×{n}") } else { line })
            .collect();
        let mut body = lines.join("\n");
        // Say what was withheld rather than silently shrinking the answer —
        // a short roster and a filtered roster must not look identical.
        if hidden > 0 {
            body.push_str(&format!(
                "\n\n  showing {} of {total} known — `--all` for the {hidden} idle",
                rows.len()
            ));
        }
        KjResult::ok_with_data(body, data)
    }

    /// `kj roster status <text> [--availability <state>]`. `caller` is the
    /// ONLY source of identity — see the module doc.
    async fn roster_status(
        &self,
        text: &str,
        availability_arg: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let entity = match caller.context_id {
            Some(ctx) => RosterEntity::Context(ctx),
            None => RosterEntity::Principal(caller.principal_id),
        };

        let previous = match self.roster().current_availability(entity) {
            Ok(v) => v,
            Err(e) => return KjResult::Err(format!("kj roster status: {e}")),
        };
        let availability = match availability_arg {
            Some(s) => match Availability::parse(s) {
                Some(a) => a,
                None => {
                    return KjResult::Err(format!(
                        "kj roster status: invalid --availability '{s}' — must be one of: \
                         active, idle, away, dnd"
                    ));
                }
            },
            // No default is applied silently on a REPEAT post: an omitted
            // flag keeps whatever the caller last reported. Only a caller's
            // very first post has nothing to keep, so it starts `active`.
            None => previous.unwrap_or(Availability::Active),
        };

        let now = kaijutsu_types::now_millis() as i64;
        if let Err(e) =
            self.roster().write_status(entity, None, Some(text), availability, Some(now), now)
        {
            return KjResult::Err(format!("kj roster status: {e}"));
        }

        // Two transition kinds can fire from one call: the status text
        // always changed (that's the point of the call), and availability
        // only when it actually moved (module doc: history is otel, not a
        // table — `kaijutsu-telemetry`'s `RosterMetrics`).
        kaijutsu_telemetry::record_roster_transition("status_changed", "");
        if previous != Some(availability) {
            kaijutsu_telemetry::record_roster_transition("availability_changed", "");
        }

        KjResult::ok(format!("status posted: \"{text}\" ({})", availability.as_str()))
    }
}

/// One roster row as `.data`/`--json` render it. Full ids only (`entity_id`
/// is the row's whole hex form) — a caller that wants to act on a row
/// programmatically (e.g. `kj roster status` for itself, or a future
/// approval-omni-view lookup) never has to round-trip a truncated display
/// id.
/// The grouping key `/run/roster` uses: entity kind + id. Presence rows are
/// per-connection, entities are what a reader means by "who".
fn entity_key(row: &crate::roster::RosterRow) -> String {
    format!("{}-{}", row.entity.kind_str(), row.entity.to_hex())
}

fn row_to_json(row: &crate::roster::RosterRow) -> serde_json::Value {
    serde_json::json!({
        "entity_kind": row.entity.kind_str(),
        "entity_id": row.entity.to_hex(),
        "label": row.label,
        "liveness_kind": row.liveness_kind.map(|k| k.as_str()),
        "live": row.live,
        "source": row.source,
        "host": row.host,
        "observed_at": row.observed_at,
        "recorded_at": row.recorded_at,
        "status_text": row.status_text,
        "availability": row.availability.map(|a| a.as_str()),
        "status_observed_at": row.status_observed_at,
        "status_recorded_at": row.status_recorded_at,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{register_context, test_dispatcher};
    use super::super::{KjCaller, KjResult};
    use crate::roster::RosterEntity;
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};

    fn caller_with_context(principal: PrincipalId, ctx: Option<ContextId>) -> KjCaller {
        KjCaller {
            principal_id: principal,
            context_id: ctx,
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        }
    }

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// A caller with no active context posts under its OWN principal — never
    /// an id it could have supplied itself, since none is accepted as an
    /// argument at all.
    #[tokio::test]
    async fn status_with_no_context_posts_under_the_callers_principal() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let caller = caller_with_context(principal, None);

        let result = d
            .dispatch(&[s("roster"), s("status"), s("heads down"), s("--availability"), s("dnd")], &caller)
            .await;
        assert!(matches!(result, KjResult::Ok { .. }), "{result:?}");

        let snap = d.roster().snapshot().unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entity, RosterEntity::Principal(principal));
        assert_eq!(snap[0].status_text.as_deref(), Some("heads down"));
    }

    /// A caller WITH an active context posts under that context's identity —
    /// the agent-reporting-on-itself case, not its principal.
    #[tokio::test]
    async fn status_with_an_active_context_posts_under_the_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        // A context row must exist for the joinability trigger.
        let ctx_id = register_context(&d, Some("roster-test-ctx"), None, principal);
        let caller = caller_with_context(principal, Some(ctx_id));

        d.dispatch(&[s("roster"), s("status"), s("building slice 3")], &caller).await;

        let snap = d.roster().snapshot().unwrap();
        let row = snap.iter().find(|r| r.entity == RosterEntity::Context(ctx_id));
        assert!(row.is_some(), "expected a roster row keyed on the context, not the principal");
        assert_eq!(row.unwrap().status_text.as_deref(), Some("building slice 3"));
    }

    /// Omitting `--availability` on a REPEAT post must not silently reset a
    /// standing `dnd` back to a default — it keeps whatever was last posted.
    #[tokio::test]
    async fn omitted_availability_keeps_the_previous_value_on_a_repeat_post() {
        let d = test_dispatcher().await;
        let caller = caller_with_context(PrincipalId::new(), None);
        d.dispatch(&[s("roster"), s("status"), s("first"), s("--availability"), s("dnd")], &caller)
            .await;
        d.dispatch(&[s("roster"), s("status"), s("second")], &caller).await;

        let snap = d.roster().snapshot().unwrap();
        assert_eq!(snap[0].status_text.as_deref(), Some("second"));
        assert_eq!(snap[0].availability, Some(crate::roster::Availability::Dnd));
    }

    /// A caller's FIRST post with no `--availability` starts `active`, never
    /// an unset/garbage value.
    #[tokio::test]
    async fn first_post_with_no_availability_defaults_to_active() {
        let d = test_dispatcher().await;
        let caller = caller_with_context(PrincipalId::new(), None);
        d.dispatch(&[s("roster"), s("status"), s("hello")], &caller).await;

        let snap = d.roster().snapshot().unwrap();
        assert_eq!(snap[0].availability, Some(crate::roster::Availability::Active));
    }

    #[tokio::test]
    async fn an_invalid_availability_is_refused_loudly() {
        let d = test_dispatcher().await;
        let caller = caller_with_context(PrincipalId::new(), None);
        let result = d
            .dispatch(
                &[s("roster"), s("status"), s("x"), s("--availability"), s("sideways")],
                &caller,
            )
            .await;
        assert!(matches!(result, KjResult::Err(_)), "{result:?}");
        assert!(d.roster().snapshot().unwrap().is_empty(), "a rejected write must not land");
    }

    /// The identity grammar has no way to name anyone else — there is no
    /// `--as`/`--principal`/`--context` flag on `kj roster status` at all,
    /// so this test simply pins that the parser rejects an attempt.
    #[tokio::test]
    async fn there_is_no_flag_to_post_as_someone_else() {
        let d = test_dispatcher().await;
        let caller = caller_with_context(PrincipalId::new(), None);
        let result = d
            .dispatch(
                &[s("roster"), s("status"), s("x"), s("--principal"), s("deadbeef")],
                &caller,
            )
            .await;
        assert!(matches!(result, KjResult::Err(_)), "expected an unrecognized-flag error, got {result:?}");
    }

    // ── list (slice 4) ─────────────────────────────────────────────────

    #[tokio::test]
    async fn list_on_a_fresh_kernel_is_empty_but_ok() {
        let d = test_dispatcher().await;
        let caller = caller_with_context(PrincipalId::new(), None);
        let result = d.dispatch(&[s("roster"), s("list")], &caller).await;
        let KjResult::Ok { message, data, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert!(message.contains("nobody"));
        assert_eq!(data, Some(serde_json::json!([])));
        // The boot rule must have run even though nothing exists yet.
        assert!(d.roster().refreshed_at().is_some());
    }

    /// `list` triggers the boot-rule refresh itself: a status post alone
    /// doesn't run any source reconciliation, but the entity still shows up
    /// once `list` runs (status-only visibility, the slice 3 fix) — this
    /// pins that `list` doesn't require a bound/recent source to have fired
    /// first.
    #[tokio::test]
    async fn list_surfaces_a_status_only_entity_and_full_ids_round_trip() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let caller = caller_with_context(principal, None);
        d.dispatch(&[s("roster"), s("status"), s("hi"), s("--availability"), s("idle")], &caller)
            .await;

        let result = d.dispatch(&[s("roster"), s("list"), s("--json")], &caller).await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(rows)), .. } = result else {
            panic!("expected a JSON array, got {result:?}");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["entity_kind"], "principal");
        // Full id, never truncated — a caller must be able to act on this
        // value without re-deriving it.
        assert_eq!(rows[0]["entity_id"], principal.to_hex());
        assert_eq!(rows[0]["status_text"], "hi");
        assert_eq!(rows[0]["availability"], "idle");
        assert_eq!(rows[0]["liveness_kind"], serde_json::Value::Null);
        assert_eq!(rows[0]["live"], serde_json::Value::Null);
    }

    /// The human-table render must not blow up on a mix of status-only and
    /// presence-carrying rows, and must include both.
    #[tokio::test]
    async fn list_human_table_renders_both_kinds_of_row() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("agent-ctx"), None, principal);
        d.dispatch(
            &[s("roster"), s("status"), s("thinking")],
            &caller_with_context(principal, None),
        )
        .await;

        let result = d.dispatch(&[s("roster"), s("list")], &caller_with_context(principal, None)).await;
        let KjResult::Ok { message, .. } = result else { panic!("expected Ok, got {result:?}") };
        assert!(message.contains("thinking"), "message: {message}");

        // The recent source picks up the registered context once refreshed.
        let snap = d.roster().snapshot().unwrap();
        assert!(snap.iter().any(|r| r.entity == RosterEntity::Context(ctx)));
    }

    /// Two connections for ONE principal render as one line, not two.
    ///
    /// `roster_presence` is keyed `(source, source_local_id)` — one row per
    /// connection — so a principal with two app windows or two MCP sessions
    /// has two rows. `/run/roster/index` has always grouped by entity; this
    /// pins `kj roster list` to the same convention, because the two surfaces
    /// disagreeing about the same data is how "the roster is flaky" starts.
    /// Found live: the same MCP principal printed twice, invisible until the
    /// idle rows stopped burying it.
    #[tokio::test]
    async fn two_connections_for_one_principal_render_as_one_row() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let entity = RosterEntity::Principal(principal);
        let mk = |local: &str, at: i64| crate::roster::PresenceSnapshotRow {
            source_local_id: local.to_string(),
            entity,
            entity_label: Some("amy".to_string()),
            host: Some("moltar".to_string()),
            pid: None,
            proc_start: None,
            observed_at: Some(at),
            live: true,
        };
        // Pin the boot rule as already satisfied: `ensure_refreshed` would
        // otherwise reconcile against this test kernel's empty peer list and
        // correctly prune these hand-written rows.
        d.roster().mark_refreshed(200);
        d.roster()
            .reconcile(
                "peer_registry",
                crate::roster::LivenessKind::Bound,
                &[mk("win-a", 100), mk("win-b", 200)],
                200,
            )
            .unwrap();

        let result =
            d.dispatch(&[s("roster"), s("list")], &caller_with_context(principal, None)).await;
        let KjResult::Ok { message, .. } = result else { panic!("expected Ok, got {result:?}") };
        let hits = message.matches(&principal.to_hex()[..8]).count()
            + message.matches("amy").count();
        assert!(
            message.contains("×2"),
            "two connections must render as one row carrying ×2; got:\n{message}"
        );
        assert_eq!(
            message.lines().filter(|l| l.contains("amy")).count(),
            1,
            "the entity must appear on exactly ONE line (hits={hits}); got:\n{message}"
        );
    }

    /// A filtered list must SAY it filtered. A short roster and a roster with
    /// 195 hidden rows must not render identically — "missing entry =
    /// unknown, never absent" applies to the count as much as to a row.
    #[tokio::test]
    async fn hiding_idle_rows_is_disclosed_and_reversible() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let entity = RosterEntity::Principal(principal);
        let mut idle = crate::roster::PresenceSnapshotRow {
            source_local_id: "win-idle".to_string(),
            entity,
            entity_label: Some("ghost".to_string()),
            host: None,
            pid: None,
            proc_start: None,
            observed_at: Some(100),
            live: false,
        };
        idle.live = false;
        d.roster().mark_refreshed(100);
        d.roster()
            .reconcile("peer_registry", crate::roster::LivenessKind::Bound, &[idle], 100)
            .unwrap();

        let caller = caller_with_context(principal, None);
        let KjResult::Ok { message, .. } =
            d.dispatch(&[s("roster"), s("list")], &caller).await
        else {
            panic!("expected Ok")
        };
        assert!(
            !message.contains("ghost"),
            "a known-idle entity is hidden by default; got:\n{message}"
        );
        assert!(
            message.contains("known"),
            "the default view must disclose what it withheld; got:\n{message}"
        );

        let KjResult::Ok { message, .. } =
            d.dispatch(&[s("roster"), s("list"), s("--all")], &caller).await
        else {
            panic!("expected Ok")
        };
        assert!(message.contains("ghost"), "--all must reveal it; got:\n{message}");
    }
}
