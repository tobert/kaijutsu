//! `kj ping` — opaque wake signal between contexts.
//!
//! A ping tells another context to take a turn now or soon without carrying
//! sender-authored text. The kernel writes a trusted `Role::System` /
//! `BlockKind::Notification` notice naming who pinged and any recent shared
//! board activity, then wakes the target through the live-input / turn
//! admission path.

use std::collections::{HashMap, VecDeque};

use clap::Parser;
use kaijutsu_types::{BlockKind, ContentType, ContextId, ContextState, PrincipalId, Role, Status, now_millis};

use super::effect::{Classify, Effect};
use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};

/// Default sliding-window rate limit: pings per `(sender, target)` pair.
pub(crate) const DEFAULT_PING_RATE_LIMIT: usize = 6;

/// Default sliding-window duration for pings (60 seconds).
pub(crate) const DEFAULT_PING_RATE_WINDOW_MS: i64 = 60_000;

/// Lookback window for recent shared board activity attached to a ping notice (15 minutes).
const BOARD_HINT_WINDOW_MS: i64 = 15 * 60 * 1000;

/// Maximum number of board topic pointers included in a single ping notice.
const MAX_BOARD_HINTS: usize = 5;

/// Per-`(sender, target)` sliding-window rate limiter for `kj ping`.
pub(crate) struct PingRateLimiter {
    inner: parking_lot::Mutex<PingRateState>,
}

struct PingRateState {
    limit: usize,
    window_ms: i64,
    history: HashMap<(PrincipalId, ContextId), VecDeque<i64>>,
}

impl Default for PingRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_PING_RATE_LIMIT, DEFAULT_PING_RATE_WINDOW_MS)
    }
}

impl PingRateLimiter {
    pub(crate) fn new(limit: usize, window_ms: i64) -> Self {
        Self {
            inner: parking_lot::Mutex::new(PingRateState {
                limit,
                window_ms,
                history: HashMap::new(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn configure(&self, limit: usize, window_ms: i64) {
        let mut state = self.inner.lock();
        state.limit = limit;
        state.window_ms = window_ms;
        state.history.clear();
    }

    pub(crate) fn check_and_record(
        &self,
        sender: PrincipalId,
        target: ContextId,
        now_ms: i64,
    ) -> Result<(), String> {
        let mut state = self.inner.lock();
        let limit = state.limit;
        let window_ms = state.window_ms;
        let cutoff = now_ms.saturating_sub(window_ms);
        let entries = state.history.entry((sender, target)).or_default();
        while entries.front().is_some_and(|&ts| ts < cutoff) {
            entries.pop_front();
        }
        if entries.len() >= limit {
            let secs = (window_ms / 1000).max(1);
            return Err(format!(
                "rate limit exceeded: at most {limit} pings per {secs}s to the same target"
            ));
        }
        entries.push_back(now_ms);
        Ok(())
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "ping",
    about = "Send an opaque wake signal to another context so it takes a turn soon.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct PingArgs {
    /// Target context to ping (label, hex prefix, `.senpai`, or `.parent`).
    target: String,
}

impl Classify for PingArgs {
    fn effect(&self) -> Effect {
        Effect::Write
    }
}

impl KjDispatcher {
    pub(crate) async fn dispatch_ping(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let parsed = match PingArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj ping: {e}"));
            }
        };

        let target = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(Some(&parsed.target), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj ping: {e}")),
            }
        };

        if Some(target) == caller.context_id {
            return KjResult::Err("kj ping: cannot ping your own context".to_string());
        }

        let (target_row, target_name, can_run_turns) = {
            let db = self.kernel_db().lock();
            let row = match db.get_context(target) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return KjResult::Err(format!("kj ping: context {} not found", target.short()));
                }
                Err(e) => {
                    return KjResult::Err(format!(
                        "kj ping: could not read context {}: {e}",
                        target.short()
                    ));
                }
            };
            let name = row
                .label
                .clone()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| target.short());
            match db.is_console(target) {
                Ok(true) => {
                    return KjResult::Err(format!(
                        "kj ping: '{name}' is a console and does not accept pings"
                    ));
                }
                Ok(false) => {}
                Err(e) => return KjResult::Err(format!("kj ping: {e}")),
            }
            if let Some(performer) = row.performer {
                match db.character_accepts_pings(performer) {
                    Ok(true) => {}
                    Ok(false) => {
                        let performer_name = db.name_for(performer);
                        return KjResult::Err(format!(
                            "kj ping: '{performer_name}' does not accept pings"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj ping: {e}")),
                }
            }
            let is_live = !row.is_archived() && row.context_state == ContextState::Live;
            let not_person = crate::runtime::turn_identity::refuse_a_persons_context(&db, target).is_ok();
            let has_performer = row.performer.is_some();
            (row, name, is_live && not_person && has_performer)
        };

        if let Err(e) = self
            .ping_rate_limiter()
            .check_and_record(caller.actor_id, target, now_millis() as i64)
        {
            return KjResult::Err(format!("kj ping: {e}"));
        }

        // Contexts that cannot run turns (archived, concluded, staging, person,
        // no performer, refusal-held, or empty) accept the ping as a no-op
        // without leaking their state to the caller.
        if !can_run_turns || self.kernel().refusal_hold(target).is_err() {
            return Self::ping_ack(target, &target_name);
        }
        if !self.block_store().contains(target)
            && self.block_store().load_one_from_db(target).is_err()
        {
            return Self::ping_ack(target, &target_name);
        }
        let Some(tail) = self.block_store().last_block_id(target) else {
            return Self::ping_ack(target, &target_name);
        };

        let notice = self.build_ping_notice(caller, &target_row);

        // If a turn is already in flight, write the trusted notice and offer it
        // to the running turn's ingress. It will be delivered on the next tool
        // round or start one coalesced follow-up turn when the current turn ends.
        if self.kernel().turn_in_flight(target) {
            let block = match self.insert_ping_block(target, &tail, &notice) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj ping: {e}")),
            };
            let live = crate::runtime::turn_state::LiveInput {
                block,
                wake: crate::runtime::turn_state::Wake::Ping {
                    principal: caller.actor_id,
                },
            };
            if self.kernel().turns().offer_input(target, live) {
                return Self::ping_ack(target, &target_name);
            }
            // The turn finished before accepting the notice; start one if still idle.
            let request = crate::runtime::turn_request::TurnRequest {
                context_id: target,
                after_block_id: block,
                content: String::new(),
                principal_id: caller.actor_id,
                continuation_epoch: None,
                score: None,
            };
            if let Err(e) = self.kernel().request_turn_if_idle(request) {
                return KjResult::Err(format!("kj ping: turn was not admitted: {e}"));
            }
            return Self::ping_ack(target, &target_name);
        }

        // Idle path: reserve runtime slot and context admission before writing
        // the block (`docs/resource-admission.md`, rule 1).
        let slot = match self.kernel().reserve_runtime_slot() {
            Ok(slot) => slot,
            Err(e) => return KjResult::Err(format!("kj ping: turn was not admitted: {e}")),
        };
        let admission = {
            let db = self.kernel_db().lock();
            match crate::runtime::admission::ContextAdmission::acquire(&db, target) {
                Ok(admission) => admission,
                Err(e) => return KjResult::Err(format!("kj ping: turn was not admitted: {e}")),
            }
        };

        let block = match self.insert_ping_block(target, &tail, &notice) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj ping: {e}")),
        };

        let request = crate::runtime::turn_request::TurnRequest {
            context_id: target,
            after_block_id: block,
            content: String::new(),
            principal_id: caller.actor_id,
            continuation_epoch: None,
            score: None,
        };
        match self.kernel().request_turn_if_idle_admitted(request, slot, admission) {
            Ok(crate::runtime::turn_request::TurnAdmission::Accepted { .. }) => {}
            Ok(crate::runtime::turn_request::TurnAdmission::AlreadyActive) => {
                // A turn started in the window between our check and admission;
                // hand the block to its ingress so the wake is never lost.
                let live = crate::runtime::turn_state::LiveInput {
                    block,
                    wake: crate::runtime::turn_state::Wake::Ping {
                        principal: caller.actor_id,
                    },
                };
                let _ = self.kernel().turns().offer_input(target, live);
            }
            Err(e) => return KjResult::Err(format!("kj ping: turn was not admitted: {e}")),
        }

        Self::ping_ack(target, &target_name)
    }

    fn ping_ack(target: ContextId, target_name: &str) -> KjResult {
        KjResult::Ok {
            message: format!("pinged '{target_name}'"),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "context_id": target.to_hex(),
            })),
        }
    }

    fn insert_ping_block(
        &self,
        target: ContextId,
        after: &kaijutsu_types::BlockId,
        notice: &str,
    ) -> Result<kaijutsu_types::BlockId, String> {
        self.block_store()
            .insert_block_as(
                target,
                None,
                Some(after),
                Role::System,
                BlockKind::Notification,
                notice.to_string(),
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::system()),
            )
            .map_err(|e| format!("failed to write ping notice: {e}"))
    }

    /// Assemble the kernel-authored notice delivered to the pinged context.
    /// Names the sender and attaches compact pointers (never bodies) to any
    /// recent posts the sender wrote on boards shared with the target.
    fn build_ping_notice(
        &self,
        caller: &KjCaller,
        target: &crate::kernel_db::ContextRow,
    ) -> String {
        let now_ms = now_millis() as i64;
        let cutoff = now_ms.saturating_sub(BOARD_HINT_WINDOW_MS);

        let (sender_desc, candidate_topics) = {
            let db = self.kernel_db().lock();
            let sender_char = db.name_for(caller.actor_id);
            let (sender_ctx_name, relation) = match caller.context_id {
                Some(src) => {
                    let src_name = db
                        .get_context(src)
                        .ok()
                        .flatten()
                        .and_then(|r| r.label)
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| src.short());
                    let src_senpai = db.senpai_of(src).ok().flatten();
                    let rel = if target.senpai_ctx == Some(src) {
                        Some("your senpai")
                    } else if src_senpai == Some(target.context_id) {
                        Some("your kohai")
                    } else if target.senpai_ctx.is_some() && target.senpai_ctx == src_senpai {
                        Some("sibling")
                    } else {
                        None
                    };
                    (Some(src_name), rel)
                }
                None => (None, None),
            };
            let desc = match (sender_ctx_name, relation) {
                (Some(ctx), Some(rel)) => format!("{ctx} ({sender_char}, {rel})"),
                (Some(ctx), None) => format!("{ctx} ({sender_char})"),
                (None, _) => sender_char.clone(),
            };

            // Collect shared boards visible to the target: its own working
            // board, its senpai's working board, and boards owned by its
            // performer character.
            let mut boards: Vec<kaijutsu_types::BoardId> = Vec::new();
            if let Some(bid) = target.board_id {
                boards.push(bid);
            }
            if let Some(senpai_ctx) = target.senpai_ctx
                && let Ok(Some(senpai_row)) = db.get_context(senpai_ctx)
                && let Some(bid) = senpai_row.board_id
                && !boards.contains(&bid)
            {
                boards.push(bid);
            }
            if let Some(performer) = target.performer
                && let Ok(char_boards) = db.list_character_boards(performer, false)
            {
                for b in char_boards {
                    if !boards.contains(&b.board_id) {
                        boards.push(b.board_id);
                    }
                }
            }

            let mut topics: Vec<(i64, String, ContextId)> = Vec::new();
            for bid in boards {
                if let Ok(rows) = db.list_topics(bid, None, None, Some(10)) {
                    for topic in rows {
                        if topic.status == kaijutsu_types::TopicStatus::Archived
                            || topic.last_post_at < cutoff
                        {
                            continue;
                        }
                        topics.push((topic.last_post_at, topic.title, topic.document_id));
                    }
                }
            }
            topics.sort_by(|a, b| b.0.cmp(&a.0));
            ((desc, sender_char), topics)
        };

        let (desc, sender_char) = sender_desc;
        let mut hints: Vec<String> = Vec::new();
        for (_ts, title, doc_id) in candidate_topics {
            if hints.len() >= MAX_BOARD_HINTS {
                break;
            }
            if !self.block_store().contains(doc_id)
                && self.block_store().load_one_from_db(doc_id).is_err()
            {
                continue;
            }
            let Ok(snaps) = self.block_store().non_draft_snapshots(doc_id) else {
                continue;
            };
            let visible: Vec<_> = snaps.into_iter().filter(|b| !b.excluded).collect();
            if let Some((idx, _)) = visible
                .iter()
                .enumerate()
                .rev()
                .find(|(_, b)| b.id.principal_id == caller.actor_id && b.created_at as i64 >= cutoff)
            {
                hints.push(format!(
                    "- \"{title}\" #{} — `kj board read '{}'`",
                    idx + 1,
                    doc_id.short()
                ));
            }
        }

        if hints.is_empty() {
            format!("Ping from {desc}.")
        } else {
            format!(
                "Ping from {desc}.\nRecent board posts by {sender_char}:\n{}",
                hints.join("\n")
            )
        }
    }
}

#[cfg(test)]
mod tests;
