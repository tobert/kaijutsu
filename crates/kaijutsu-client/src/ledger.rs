//! The approval ledger round trip: list pending asks, read one ask's
//! fields, and write back an allow/deny decision — the machinery every
//! client that offers asks to a player drives the same way.
//!
//! This is not a bespoke wire: there is no `PermissionEvents::onAsk`
//! (`docs/gate-and-shell-split.md`, "The shared seam: one ledger, one
//! announcement, one write path"). The ledger is the one durable record and
//! `kj ledger` is the one write path, from any surface. This module drives
//! that path through [`ActorHandle::execute_kj`] and
//! [`ActorHandle::execute_kj_quiet`]; it opens no bespoke connection and
//! holds no state beyond the `seen` set a caller passes in.
//!
//! # The kernel is the authority, and nothing expires
//!
//! There is no ask-timeout budget owned here, and there is no kernel-side
//! one either: the gate records an ask and returns, and an unanswered ask
//! stays answerable indefinitely (`docs/gate-resume.md`). Bounding an
//! outgoing round trip to a *player* (a client waiting on a human) is each
//! caller's own concern, not this module's.
//!
//! # Reads are quiet; the decision is not
//!
//! `kj ledger list` and `show` run through
//! [`ActorHandle::execute_kj_quiet`] and author no blocks — this is the
//! client's own bookkeeping, not a player's command, and polling it on a
//! timer must not fill the transcript. [`decide_ask`] and
//! [`decide_ask_remember`] run through [`ActorHandle::execute_kj`] instead:
//! an allow/deny is a real decision and leaves a tool-call/tool-result pair
//! in the answering context. Drive [`poll_new_asks`] from
//! `ActorHandle::subscribe_ledger_events` (one poll per generation bump) plus
//! one poll at startup, never from a clock.
//!
//! # Racing is fine and expected
//!
//! A human can answer the same ask with `kj ledger allow` from a shell while
//! another surface's prompt is still on someone's screen — the ledger's
//! `claim`+`decide` transaction makes exactly one answerer win
//! (`approval-ledger`'s guarantee 5). The loser's [`decide_ask`] comes back
//! with a nonzero exit code (`AlreadyDecided`); this is not a failure, it is
//! two players sharing one ledger, and it is the caller's call whether to
//! log it.

use std::collections::HashSet;

use kaijutsu_types::{ContextId, PrincipalId};

use crate::actor::{ActorHandle, CallError};
use crate::rpc::KjExecutionResult;

/// One pending ask's `kj ledger show` fields, decoded once so callers don't
/// hand around a raw `serde_json::Value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskInfo {
    pub context_id: ContextId,
    pub description: String,
}

/// One ask [`poll_new_asks`] has not offered to the caller before: its
/// request id plus the fields [`show_ask`] decoded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub request_id: String,
    pub info: AskInfo,
}

/// One authoritative pending-ledger snapshot plus asks not yet presented by
/// this client. `pending_ids` is not presentation state: callers use it to
/// retain indicators for asks that are still waiting behind another card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAskPoll {
    pub pending_ids: HashSet<String>,
    pub new_asks: Vec<PendingAsk>,
}

/// Failure decoding a `kj ledger` round trip: either the RPC call itself
/// failed, or it ran and the verb reported a nonzero exit.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Call(#[from] CallError),
    #[error("kj ledger {verb} exited {exit_code}: {stderr}")]
    Failed {
        verb: &'static str,
        exit_code: i32,
        stderr: String,
    },
}

/// List every pending ask's request id, via `kj ledger list` run in `ctx`
/// (the ledger is kernel-wide state, so which live context the command runs
/// in doesn't matter).
pub async fn list_pending(actor: &ActorHandle, ctx: ContextId) -> Result<Vec<String>, LedgerError> {
    let result = actor
        .execute_kj_quiet(ctx, vec!["ledger".to_string(), "list".to_string()])
        .await?;
    if result.exit_code != 0 {
        return Err(LedgerError::Failed {
            verb: "list",
            exit_code: result.exit_code,
            stderr: result.stderr,
        });
    }
    let ids = match result.data {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Ok(ids)
}

/// Read one ask's context id and description via `kj ledger show`. `None`
/// on any failure to run or parse — a caller should treat that as "try
/// again next poll", not as a decision (see [`poll_new_asks`]).
pub async fn show_ask(actor: &ActorHandle, ctx: ContextId, request_id: &str) -> Option<AskInfo> {
    let result = match actor
        .execute_kj_quiet(
            ctx,
            vec!["ledger".to_string(), "show".to_string(), request_id.to_string()],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(request = %request_id, error = %e, "kj ledger show errored");
            return None;
        }
    };
    if result.exit_code != 0 {
        tracing::warn!(
            request = %request_id,
            exit_code = result.exit_code,
            stderr = %result.stderr,
            "kj ledger show failed"
        );
        return None;
    }
    let data = result.data?;
    let context_id = match data
        .get("context_id")
        .and_then(|v| v.as_str())
        .and_then(|s| ContextId::parse(s).ok())
    {
        Some(id) => id,
        None => {
            tracing::warn!(request = %request_id, "ledger ask has no parseable context_id; skipping");
            return None;
        }
    };
    let description = data
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    Some(AskInfo {
        context_id,
        description,
    })
}

/// Write an allow/deny decision back through `kj ledger allow|deny`. Only
/// the RPC round trip can fail here — a nonzero exit (a race lost to
/// another answerer, or an expired ask) comes back as `Ok` with that exit
/// code in [`KjExecutionResult`], for the caller to log or ignore as it
/// sees fit (see module docs, "Racing is fine").
pub async fn decide_ask(
    actor: &ActorHandle,
    ctx: ContextId,
    request_id: &str,
    allow: bool,
) -> Result<KjExecutionResult, CallError> {
    let verb = if allow { "allow" } else { "deny" };
    actor
        .execute_kj(
            ctx,
            vec!["ledger".to_string(), verb.to_string(), request_id.to_string()],
        )
        .await
}

/// Prune `seen` down to the ids still pending, and return the pending ids
/// not yet in `seen`, in list order. Pure and unit-testable without a
/// kernel — the one decision in [`poll_new_asks`] that isn't an RPC round
/// trip.
fn diff_new(ids: &[String], seen: &HashSet<String>) -> Vec<String> {
    ids.iter().filter(|id| !seen.contains(id.as_str())).cloned().collect()
}

/// One poll of the ledger: list pending asks and read the fields of every ask
/// not yet in `seen`. The returned snapshot lets callers prune presentation
/// state without mistaking it for the ledger's pending set.
///
/// This does **not** insert into `seen` — that is the caller's job, once it
/// has decided which of the returned asks it will actually offer (e.g.
/// filtering by whether it owns the ask's context). An ask this call
/// returns but the caller declines to answer must stay out of `seen`, so it
/// is offered again if ownership changes on a later poll.
///
/// An ask whose `kj ledger show` fails is left out of both `seen` and the
/// returned list — a transient read failure retries next poll rather than
/// permanently suppressing the ask.
pub async fn poll_new_asks(
    actor: &ActorHandle,
    ctx: ContextId,
    seen: &HashSet<String>,
) -> Result<PendingAskPoll, LedgerError> {
    let ids = list_pending(actor, ctx).await?;
    let new_ids = diff_new(&ids, seen);

    let mut new_asks = Vec::with_capacity(new_ids.len());
    for id in new_ids {
        if let Some(info) = show_ask(actor, ctx, &id).await {
            new_asks.push(PendingAsk { request_id: id, info });
        }
    }
    Ok(PendingAskPoll { pending_ids: ids.into_iter().collect(), new_asks })
}

/// List every asks id in the decided history (`kj ledger list --history`):
/// allowed, denied, expired, abandoned, most recently created first. The
/// TUI's ledger view ANSWERED section (`docs/tui.md`, "The ledger") reads
/// this the same way [`list_pending`] feeds PENDING.
pub async fn list_history(actor: &ActorHandle, ctx: ContextId) -> Result<Vec<String>, LedgerError> {
    list_ids(actor, ctx, &["--history"]).await
}

/// Shared body of [`list_pending`] and [`list_history`]: run `kj ledger
/// list` with `extra_args` appended, and decode `.data`'s flat array of
/// request-id strings.
async fn list_ids(
    actor: &ActorHandle,
    ctx: ContextId,
    extra_args: &[&str],
) -> Result<Vec<String>, LedgerError> {
    let mut argv = vec!["ledger".to_string(), "list".to_string()];
    argv.extend(extra_args.iter().map(|s| s.to_string()));
    let result = actor.execute_kj_quiet(ctx, argv).await?;
    if result.exit_code != 0 {
        return Err(LedgerError::Failed {
            verb: "list",
            exit_code: result.exit_code,
            stderr: result.stderr,
        });
    }
    let ids = match result.data {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Ok(ids)
}

/// One free variable's recorded value on an ask (`approval_env`), as `kj
/// ledger show`'s `.data.env` carries it: a row exists for every free
/// variable name the ask's statements read, `value: None` meaning it was
/// unset at ask time (not "no snapshot taken").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: Option<String>,
}

/// One ask's full detail, decoded from `kj ledger show`'s `.data` — every
/// field that JSON carries today (`kaijutsu-kernel/src/kj/ledger.rs`,
/// `ledger_show`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskDetail {
    pub request_id: String,
    pub context_id: Option<ContextId>,
    /// The authenticated principal that submitted the request. This is
    /// distinct from the actor that performed the work: a person can direct
    /// a coder character, or a lead character can direct another character.
    pub principal_id: Option<PrincipalId>,
    /// The current name for [`Self::principal_id`], if its character sheet
    /// still exists.
    pub principal_name: Option<String>,
    /// The character that performed the work which raised this ask.
    pub actor_id: Option<PrincipalId>,
    /// The current name for [`Self::actor_id`].
    pub actor_name: Option<String>,
    /// The character the delegation names as the eligible reviewer.
    pub reviewer_id: Option<PrincipalId>,
    /// The current name for [`Self::reviewer_id`].
    pub reviewer_name: Option<String>,
    pub status: String,
    pub origin: String,
    /// Ledger `tool` column, e.g. `"shell_write"` — the figure in
    /// `docs/tui.md`'s Asks section calls this the ask's "hook", but the
    /// wire field it reads is `tool`, not `hook_id` (`hook_id` is the rc
    /// hook, when one raised the ask, and is often empty for a shell gate).
    pub tool: Option<String>,
    pub hook_id: Option<String>,
    pub instance: Option<String>,
    pub description: String,
    pub authorized_label: Option<String>,
    pub statements: Vec<String>,
    pub exec_source: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<EnvVar>,
    /// When the ask was raised, unix-epoch milliseconds.
    pub created_at: Option<i64>,
    /// When it was decided, unix-epoch milliseconds; `None` while pending.
    pub decided_at: Option<i64>,
    /// Who decided it; `None` while pending or when a rule auto-decided it.
    pub decided_by: Option<PrincipalId>,
    /// The current name for [`Self::decided_by`].
    pub decided_by_name: Option<String>,
    /// `allow_once` / `allow_always` / `deny` / `auto_allow`; `None` while
    /// pending. Finer than `status`, which says only `allowed`/`denied`.
    pub decided_option: Option<String>,
    pub remember_scope: Option<String>,
    /// When this decision was redeemed (actually executed), or `None` when
    /// it never was, or the ask is still pending. `docs/tui.md`'s "was this
    /// consumed" question.
    pub redeemed_at: Option<i64>,
}

impl AskDetail {
    /// Whether `principal` is the reviewer this ask snapshots, while the ask
    /// still names a distinct performer. Legacy or incomplete rows carry no
    /// review authority.
    pub fn can_review(&self, principal: PrincipalId) -> bool {
        matches!(
            (self.actor_id, self.reviewer_id),
            (Some(actor), Some(reviewer)) if principal == reviewer && principal != actor
        )
    }
}

/// Read one ask's full detail via `kj ledger show`. `Ok(None)` when the
/// round trip succeeded but `.data` did not decode — a caller should treat
/// that as "nothing to show", not as a decision, the same discipline
/// [`show_ask`] applies to its own decode failures.
pub async fn show_ask_detail(
    actor: &ActorHandle,
    ctx: ContextId,
    request_id: &str,
) -> Result<Option<AskDetail>, LedgerError> {
    let result = actor
        .execute_kj_quiet(
            ctx,
            vec!["ledger".to_string(), "show".to_string(), request_id.to_string()],
        )
        .await?;
    if result.exit_code != 0 {
        return Err(LedgerError::Failed {
            verb: "show",
            exit_code: result.exit_code,
            stderr: result.stderr,
        });
    }
    let Some(data) = result.data else {
        tracing::warn!(request = %request_id, "kj ledger show returned no data");
        return Ok(None);
    };
    Ok(decode_ask_detail(&data))
}

fn decode_ask_detail(data: &serde_json::Value) -> Option<AskDetail> {
    let request_id = data.get("request_id")?.as_str()?.to_string();
    let str_field = |key: &str| data.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let statements = data
        .get("statements")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let env = data
        .get("env")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    let value = item.get("value").and_then(|v| v.as_str()).map(str::to_string);
                    Some(EnvVar { name, value })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(AskDetail {
        request_id,
        context_id: data
            .get("context_id")
            .and_then(|v| v.as_str())
            .and_then(|s| ContextId::parse(s).ok()),
        principal_id: str_field("principal_id").and_then(|s| PrincipalId::parse(&s).ok()),
        principal_name: str_field("principal_name"),
        actor_id: str_field("actor_id").and_then(|s| PrincipalId::parse(&s).ok()),
        actor_name: str_field("actor_name"),
        reviewer_id: str_field("reviewer_id").and_then(|s| PrincipalId::parse(&s).ok()),
        reviewer_name: str_field("reviewer_name"),
        status: str_field("status").unwrap_or_default(),
        origin: str_field("origin").unwrap_or_default(),
        tool: str_field("tool"),
        hook_id: str_field("hook_id"),
        instance: str_field("instance"),
        description: str_field("description").unwrap_or_default(),
        authorized_label: str_field("authorized_label"),
        statements,
        exec_source: str_field("exec_source"),
        cwd: str_field("cwd"),
        env,
        created_at: data.get("created_at").and_then(|v| v.as_i64()),
        decided_at: data.get("decided_at").and_then(|v| v.as_i64()),
        decided_by: str_field("decided_by").and_then(|s| PrincipalId::parse(&s).ok()),
        decided_by_name: str_field("decided_by_name"),
        decided_option: str_field("decided_option"),
        remember_scope: str_field("remember_scope"),
        redeemed_at: data.get("redeemed_at").and_then(|v| v.as_i64()),
    })
}

/// `--remember <scope>` on `kj ledger allow|deny`. `Session` covers the
/// original ask; `Always` creates a global remembered rule for matching
/// statements and labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberScope {
    Session,
    Always,
}

impl RememberScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Always => "always",
        }
    }
}

/// The argv `kj ledger allow|deny <id> [--remember <scope>]` builds. Pure,
/// so the once/always/deny mapping the ledger view's `a`/`A`/`d` keys drive
/// is unit-tested without a kernel.
fn decide_argv(request_id: &str, allow: bool, remember: Option<RememberScope>) -> Vec<String> {
    let mut argv = vec![
        "ledger".to_string(),
        if allow { "allow" } else { "deny" }.to_string(),
        request_id.to_string(),
    ];
    if let Some(scope) = remember {
        argv.push("--remember".to_string());
        argv.push(scope.as_str().to_string());
    }
    argv
}

/// Write an allow/deny decision back through `kj ledger allow|deny`, same
/// contract as [`decide_ask`] (only the RPC round trip can fail here — a
/// nonzero exit is a race or an expired ask, not a [`CallError`]), with the
/// `--remember` option [`decide_ask`] does not carry — the ledger view's
/// `[A]llow always` key (`docs/tui.md`, "Asks").
pub async fn decide_ask_remember(
    actor: &ActorHandle,
    ctx: ContextId,
    request_id: &str,
    allow: bool,
    remember: Option<RememberScope>,
) -> Result<KjExecutionResult, CallError> {
    actor.execute_kj(ctx, decide_argv(request_id, allow, remember)).await
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn decide_argv_allow_once_carries_no_remember_flag() {
        assert_eq!(
            decide_argv("req-1", true, None),
            vec!["ledger", "allow", "req-1"]
        );
    }

    #[test]
    fn decide_argv_deny_carries_no_remember_flag() {
        assert_eq!(decide_argv("req-1", false, None), vec!["ledger", "deny", "req-1"]);
    }

    #[test]
    fn decide_argv_allow_always_appends_remember_always() {
        assert_eq!(
            decide_argv("req-1", true, Some(RememberScope::Always)),
            vec!["ledger", "allow", "req-1", "--remember", "always"]
        );
    }

    /// The full shape `ledger_show` actually emits
    /// (`kaijutsu-kernel/src/kj/ledger.rs`, `ledger_show`'s `data =
    /// serde_json::json!({...})`), so a wire-shape drift there fails this
    /// test rather than surfacing as a client that silently shows nothing.
    fn full_show_data() -> serde_json::Value {
        serde_json::json!({
            "request_id": "01a04eb6-aaaa-bbbb-cccc-000000000001",
            "context_id": "0198f2b0-0000-7000-8000-000000000001",
            "principal_id": "0198f2b0-0000-7000-8000-000000000002",
            "principal_name": "amy",
            "actor_id": "0198f2b0-0000-7000-8000-000000000004",
            "actor_name": "coder",
            "reviewer_id": "0198f2b0-0000-7000-8000-000000000003",
            "reviewer_name": "amy",
            "created_at": 1_756_819_300_000i64,
            "decided_at": 1_756_819_330_000i64,
            "decided_by": "0198f2b0-0000-7000-8000-000000000003",
            "decided_by_name": "amy",
            "decided_option": "allow_once",
            "remember_scope": null,
            "redeemed_at": 1_756_819_331_000i64,
            "status": "allowed",
            "origin": "shell_gate",
            "instance": "kaish-1",
            "tool": "shell_write",
            "hook_id": null,
            "description": "rm -rf ~/src/wt/kaish-arith",
            "authorized_label": "worktree-remove",
            "statements": ["rm -rf ~/src/wt/kaish-arith"],
            "exec_source": "kaish",
            "cwd": "/home/amy/src/wt/kaish-arith",
            "env": [{"name": "TARGET", "value": "kaish-arith"}, {"name": "FORCE", "value": null}],
        })
    }

    #[test]
    fn decode_ask_detail_reads_every_field() {
        let detail = decode_ask_detail(&full_show_data()).expect("decodes");
        assert_eq!(detail.request_id, "01a04eb6-aaaa-bbbb-cccc-000000000001");
        assert!(detail.context_id.is_some());
        assert_eq!(detail.principal_name.as_deref(), Some("amy"));
        assert_eq!(detail.actor_name.as_deref(), Some("coder"));
        assert_eq!(detail.reviewer_name.as_deref(), Some("amy"));
        assert_eq!(detail.status, "allowed");
        assert_eq!(detail.origin, "shell_gate");
        assert_eq!(detail.tool.as_deref(), Some("shell_write"));
        assert_eq!(detail.hook_id, None);
        assert_eq!(detail.description, "rm -rf ~/src/wt/kaish-arith");
        assert_eq!(detail.statements, vec!["rm -rf ~/src/wt/kaish-arith".to_string()]);
        assert_eq!(detail.exec_source.as_deref(), Some("kaish"));
        assert_eq!(detail.cwd.as_deref(), Some("/home/amy/src/wt/kaish-arith"));
        assert_eq!(detail.redeemed_at, Some(1_756_819_331_000));
        assert_eq!(detail.created_at, Some(1_756_819_300_000));
        assert_eq!(detail.decided_at, Some(1_756_819_330_000));
        assert_eq!(
            detail.decided_by.map(|p| p.to_string()).as_deref(),
            Some("0198f2b0-0000-7000-8000-000000000003")
        );
        assert_eq!(detail.decided_option.as_deref(), Some("allow_once"));
        assert_eq!(detail.decided_by_name.as_deref(), Some("amy"));
        assert_eq!(detail.remember_scope, None);
        assert_eq!(
            detail.env,
            vec![
                EnvVar { name: "TARGET".to_string(), value: Some("kaish-arith".to_string()) },
                EnvVar { name: "FORCE".to_string(), value: None },
            ]
        );
    }

    #[test]
    fn ask_review_requires_complete_distinct_identity_assignment() {
        let detail = decode_ask_detail(&full_show_data()).expect("decodes");
        let actor = detail.actor_id.expect("fixture actor");
        let reviewer = detail.reviewer_id.expect("fixture reviewer");
        assert!(detail.can_review(reviewer));
        assert!(!detail.can_review(actor));
        assert!(!detail.can_review(PrincipalId::new()), "an unknown principal cannot review");

        let mut missing_actor = detail.clone();
        missing_actor.actor_id = None;
        assert!(!missing_actor.can_review(reviewer), "missing actor is unresolved identity");

        let mut missing_reviewer = detail;
        missing_reviewer.reviewer_id = None;
        assert!(!missing_reviewer.can_review(reviewer), "missing reviewer is unresolved identity");
    }

    #[test]
    fn decode_ask_detail_rejects_a_value_with_no_request_id() {
        assert!(decode_ask_detail(&serde_json::json!({ "status": "pending" })).is_none());
    }

    #[test]
    fn decode_ask_detail_tolerates_missing_optional_fields() {
        let detail = decode_ask_detail(&serde_json::json!({ "request_id": "r1" })).expect("decodes");
        assert_eq!(detail.request_id, "r1");
        assert_eq!(detail.context_id, None);
        assert_eq!(detail.status, "");
        assert!(detail.statements.is_empty());
        assert!(detail.env.is_empty());
        assert_eq!(detail.redeemed_at, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_new_reports_ids_not_yet_seen() {
        let mut seen = HashSet::new();
        seen.insert("a".to_string());
        let ids = vec!["a".to_string(), "b".to_string()];
        assert_eq!(diff_new(&ids, &seen), vec!["b".to_string()]);
    }

    #[test]
    fn diff_new_prunes_seen_ids_that_are_no_longer_pending() {
        let mut seen = HashSet::new();
        seen.insert("answered".to_string());
        seen.insert("still-pending".to_string());
        let ids = vec!["still-pending".to_string()];

        assert_eq!(diff_new(&ids, &seen), Vec::<String>::new());
        assert_eq!(seen, HashSet::from(["answered".to_string(), "still-pending".to_string()]));
    }

    #[test]
    fn diff_new_with_nothing_seen_returns_everything() {
        let mut seen = HashSet::new();
        let ids = vec!["x".to_string(), "y".to_string()];
        assert_eq!(diff_new(&ids, &seen), ids);
    }
}
