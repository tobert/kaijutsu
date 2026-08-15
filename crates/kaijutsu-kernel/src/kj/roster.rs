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
        }
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
}
