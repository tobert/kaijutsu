//! `kj board` — shared topic boards and exomemory (`docs/board.md`).
//!
//! A board is a container of topics owned by either a character (durable named
//! boards) or a context (a lazily created working board reached via `.` or
//! `.senpai`). Each topic is a `DocKind::Topic` document in `BlockStore` paired
//! with a `board_topics` row in `KernelDb`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;

use clap::{Parser, Subcommand};
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, BoardId, BoardWritePolicy, ContentType, ContextId, DocKind,
    PrincipalId, Role, Status, TopicStatus, now_millis,
};
use regex::Regex;

use super::effect::{Classify, Effect};
use super::gate_policy::ConfigTierOverride;
use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::kernel_db::{BoardRow, BoardTopicRow, CharacterRow, KernelDb};

/// Maximum post size in bytes (32 KiB).
pub(crate) const MAX_POST_BYTES: usize = 32 * 1024;

/// Default sliding-window rate limit on `open` boards: posts per character per board.
pub(crate) const DEFAULT_BOARD_RATE_LIMIT: usize = 20;

/// Default sliding-window duration on `open` boards (60 seconds).
pub(crate) const DEFAULT_BOARD_RATE_WINDOW_MS: i64 = 60_000;

/// Maximum preview length (in Unicode scalar values) for backref headers.
const BACKREF_PREVIEW_CHARS: usize = 60;

/// Per-character sliding-window rate limiter for `open` boards.
pub(crate) struct BoardRateLimiter {
    inner: parking_lot::Mutex<BoardRateState>,
}

struct BoardRateState {
    limit: usize,
    window_ms: i64,
    history: HashMap<(BoardId, PrincipalId), VecDeque<i64>>,
}

impl Default for BoardRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_BOARD_RATE_LIMIT, DEFAULT_BOARD_RATE_WINDOW_MS)
    }
}

impl BoardRateLimiter {
    pub(crate) fn new(limit: usize, window_ms: i64) -> Self {
        Self {
            inner: parking_lot::Mutex::new(BoardRateState {
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
        board_id: BoardId,
        principal_id: PrincipalId,
        now_ms: i64,
    ) -> Result<(), String> {
        let mut state = self.inner.lock();
        let limit = state.limit;
        let window_ms = state.window_ms;
        let cutoff = now_ms.saturating_sub(window_ms);
        let entries = state.history.entry((board_id, principal_id)).or_default();
        while entries.front().is_some_and(|&ts| ts < cutoff) {
            entries.pop_front();
        }
        if entries.len() >= limit {
            let secs = (window_ms / 1000).max(1);
            return Err(format!(
                "rate limit exceeded: at most {limit} posts per {secs}s per character per board"
            ));
        }
        entries.push_back(now_ms);
        Ok(())
    }
}

/// Settle an answered `Origin::Board` ask: an allow releases the held post
/// (`Status::Done`, `excluded = false`, emitting `StatusChanged` for the
/// semantic index and updating `board_topics.last_post_at`), while a denial or
/// cancellation dismisses it (`Status::Done`, keeping `excluded = true`).
/// Idempotent if already settled.
pub(crate) fn settle_board_post_answer(
    kernel: &crate::Kernel,
    request_id: &str,
    allowed: bool,
) -> Result<(), String> {
    let post = kernel
        .kernel_db()
        .lock()
        .approval_board_post(request_id)
        .map_err(|e| format!("could not read the board ask's post: {e}"))?
        .ok_or_else(|| format!("board ask {request_id} records no post"))?;
    let blocks = kernel.blocks();
    if !blocks.contains(post.document_id) {
        blocks
            .load_one_from_db(post.document_id)
            .map_err(|e| format!("could not load topic {}: {e}", post.document_id.short()))?;
    }
    let held = blocks
        .get_block_snapshot(post.document_id, &post.block_id)
        .map_err(|e| format!("could not read the board post block: {e}"))?
        .is_some_and(|b| b.status == Status::Pending && b.excluded);
    if held {
        blocks
            .settle_held_board_post(post.document_id, &post.block_id, allowed)
            .map_err(|e| format!("could not settle the held board post: {e}"))?;
    }
    if allowed {
        let visible_count = blocks
            .non_draft_snapshots(post.document_id)
            .map(|snaps| snaps.iter().filter(|b| !b.excluded).count() as i64)
            .unwrap_or(1);
        kernel
            .kernel_db()
            .lock()
            .update_topic_on_post_release(
                post.document_id,
                visible_count,
                now_millis() as i64,
            )
            .map_err(|e| format!("could not update topic stats on release: {e}"))?;
    }
    kernel
        .kernel_db()
        .lock()
        .redeem_ask(request_id)
        .map_err(|e| format!("could not spend the board ask's answer: {e}"))?;
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "board",
    about = "Read, post, search, and moderate shared boards and topics",
    after_help = "Board refs: . (this context), .senpai (senpai context), '<slug>', 'self/<slug>', '<character>/<slug>'.\nTopic refs: './<slug>', '.senpai/<slug>', '<character>/<board>/<topic>', '<board>/<topic>', or '<hex-prefix>'.\nPost refs: '<topic>#<n>' (1-based post number). Quote refs with '#' or leading digits in single quotes.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct BoardArgs {
    #[command(subcommand)]
    command: BoardCommand,
}

#[derive(Subcommand, Debug)]
enum BoardCommand {
    /// List boards owned by a character (defaults to the caller's character).
    #[command(alias = "ls")]
    List {
        /// Character name; defaults to the caller's character.
        character: Option<String>,
    },
    /// List topics on a board, pinned first then most recently posted.
    Topics {
        /// Board ref: ., .senpai, '<slug>', 'self/<slug>', or '<character>/<slug>'.
        board: String,
        /// Filter by status: open, resolved, pinned, archived, or all (default excludes archived).
        #[arg(long, value_parser = ["open", "resolved", "pinned", "archived", "all"])]
        status: Option<String>,
        /// Show only topics carrying this tag.
        #[arg(long)]
        tag: Option<String>,
        /// Maximum number of topics to return. Default 20.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Include topics whose posts are all hidden or held pending review.
        #[arg(long = "include-hidden")]
        include_hidden: bool,
    },
    /// Read posts in a topic, numbered #1, #2, ... in log order.
    Read {
        /// Topic ref: './<slug>', '.senpai/<slug>', '<character>/<board>/<topic>', or '<hex-prefix>'.
        topic: String,
        /// Show the last N visible posts (ignored when --after is set). Default 20.
        #[arg(long, default_value_t = 20)]
        tail: usize,
        /// Show visible posts strictly after 1-based post number N.
        #[arg(long)]
        after: Option<usize>,
        /// Include posts hidden by moderation.
        #[arg(long = "include-hidden")]
        include_hidden: bool,
    },
    /// List recently active non-archived topics across boards or on one board.
    Recent {
        /// Scope to one board ref.
        #[arg(long)]
        board: Option<String>,
        /// Time window back from now (e.g. '30m', '24h', '7d', '60s'). Default '24h'.
        #[arg(long, default_value = "24h")]
        since: String,
        /// Maximum number of topics to return. Default 20.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Include topics whose posts are all hidden or held pending review.
        #[arg(long = "include-hidden")]
        include_hidden: bool,
    },
    /// Search non-archived topics by regex (default) or semantic similarity.
    Search {
        /// Regex pattern (Rust regex syntax) or natural-language query with --semantic.
        query: String,
        /// Scope search to one board ref.
        #[arg(long)]
        board: Option<String>,
        /// Search by semantic similarity through the kernel semantic index.
        #[arg(long, conflicts_with = "regex")]
        semantic: bool,
        /// Search post text by Rust regular expression (default).
        #[arg(long, conflicts_with = "semantic")]
        regex: bool,
        /// Maximum number of results to return. Default 10.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Create a named board owned by a character.
    Create {
        /// URL-safe board slug (ASCII letters, digits, '-', '_').
        slug: String,
        /// Owning character name (defaults to the caller's character; another character requires senpai or house).
        #[arg(long = "for")]
        for_character: Option<String>,
        /// Write policy: open, curated, or locked. Default 'open'.
        #[arg(long, default_value = "open", value_parser = ["open", "curated", "locked"])]
        policy: String,
        /// Short board description.
        #[arg(long, default_value = "")]
        desc: String,
    },
    /// Update a board's write policy or description.
    Set {
        /// Board ref to modify.
        board: String,
        /// New write policy: open, curated, or locked.
        #[arg(long, value_parser = ["open", "curated", "locked"])]
        policy: Option<String>,
        /// New board description.
        #[arg(long)]
        desc: Option<String>,
    },
    /// Start a new topic on a board with an opening post (max 32 KiB; reads stdin when body is omitted).
    Post {
        /// Target board ref (lazily creates a context board for . or .senpai).
        board: String,
        /// Topic title.
        #[arg(long)]
        title: String,
        /// Topic tag (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Draft the opening post in the kernel vi editor; `:w` posts it.
        #[arg(long)]
        vi: bool,
        /// Body text injected from stdin in kaish; explicit positional body takes precedence.
        #[arg(long, hide = true)]
        content: Option<String>,
        /// Opening post text (all remaining words joined with spaces; reads stdin when omitted).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        body: Vec<String>,
    },
    /// Append a reply post to a topic (max 32 KiB; reads stdin when body is omitted).
    Reply {
        /// Target topic ref.
        topic: String,
        /// 1-based post number to reference in a backref header.
        #[arg(long)]
        to: Option<usize>,
        /// Draft the reply in the kernel vi editor; `:w` posts it.
        #[arg(long)]
        vi: bool,
        /// Body text injected from stdin in kaish; explicit positional body takes precedence.
        #[arg(long, hide = true)]
        content: Option<String>,
        /// Reply text (all remaining words joined with spaces; reads stdin when omitted).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        body: Vec<String>,
    },
    /// Replace the text of an existing post (max 32 KiB; reads stdin when body is omitted).
    Edit {
        /// Post ref: '<topic>#<n>' (1-based post number).
        post: String,
        /// Edit the post in the kernel vi editor; `:w` saves the edit.
        #[arg(long)]
        vi: bool,
        /// Body text injected from stdin in kaish; explicit positional body takes precedence.
        #[arg(long, hide = true)]
        content: Option<String>,
        /// Replacement post text (all remaining words joined with spaces; reads stdin when omitted).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        body: Vec<String>,
    },
    /// Mark a topic resolved.
    Resolve {
        /// Topic ref to resolve.
        topic: String,
    },
    /// Reopen a resolved or archived topic.
    Reopen {
        /// Topic ref to reopen.
        topic: String,
    },
    /// Archive a topic while retaining all posts.
    Archive {
        /// Topic ref to archive.
        topic: String,
    },
    /// Pin a topic to the top of its board listing.
    Pin {
        /// Topic ref to pin.
        topic: String,
    },
    /// Unpin a pinned topic back to open status.
    Unpin {
        /// Topic ref to unpin.
        topic: String,
    },
    /// Lock a topic so no further replies can be posted.
    Lock {
        /// Topic ref to lock.
        topic: String,
    },
    /// Unlock a locked topic so replies can resume.
    Unlock {
        /// Topic ref to unlock.
        topic: String,
    },
    /// Hide a post from default reads and indexing (retains the block).
    Hide {
        /// Post ref: '<topic>#<n>' (1-based post number).
        post: String,
    },
    /// Restore a hidden post to default reads and indexing.
    Unhide {
        /// Post ref: '<topic>#<n>' (1-based post number).
        post: String,
    },
    /// Promote a topic to another board, copying its visible posts or distilling them with --summarize.
    Promote {
        /// Source topic ref to promote.
        topic: String,
        /// Target board ref (lazily creates a context board for . or .senpai).
        #[arg(long)]
        to: String,
        /// Title for the promoted topic (defaults to the source topic's title).
        #[arg(long)]
        title: Option<String>,
        /// Distill the source topic using the caller's `distill` slot instead of copying posts verbatim.
        #[arg(long)]
        summarize: bool,
    },
}

impl Classify for BoardArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for BoardCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::List { .. }
            | Self::Topics { .. }
            | Self::Read { .. }
            | Self::Recent { .. }
            | Self::Search { .. } => Effect::Read,
            Self::Create { .. }
            | Self::Set { .. }
            | Self::Post { .. }
            | Self::Reply { .. }
            | Self::Edit { .. }
            | Self::Resolve { .. }
            | Self::Reopen { .. }
            | Self::Archive { .. }
            | Self::Pin { .. }
            | Self::Unpin { .. }
            | Self::Lock { .. }
            | Self::Unlock { .. }
            | Self::Hide { .. }
            | Self::Unhide { .. }
            | Self::Promote { .. } => Effect::Write,
        }
    }
}

/// Outcome of resolving a board ref for reading.
enum ReadBoard {
    Found(BoardRow),
    /// Context board (`.` or `.senpai`) has not been lazily created yet.
    UncreatedContextBoard,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_board(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<BoardArgs>();
        }
        let parsed = match BoardArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj board: {e}"));
            }
        };

        match parsed.command {
            BoardCommand::List { character } => self.board_list(character.as_deref(), caller),
            BoardCommand::Topics { board, status, tag, limit, include_hidden } => {
                self.board_topics(&board, status.as_deref(), tag.as_deref(), limit, include_hidden, caller)
            }
            BoardCommand::Read { topic, tail, after, include_hidden } => {
                self.board_read(&topic, tail, after, include_hidden, caller)
            }
            BoardCommand::Recent { board, since, limit, include_hidden } => {
                self.board_recent(board.as_deref(), &since, limit, include_hidden, caller)
            }
            BoardCommand::Search { query, board, semantic, regex: _, limit } => {
                self.board_search(&query, board.as_deref(), semantic, limit, caller).await
            }
            BoardCommand::Create { slug, for_character, policy, desc } => {
                self.board_create(&slug, for_character.as_deref(), &policy, &desc, caller)
            }
            BoardCommand::Set { board, policy, desc } => {
                self.board_set(&board, policy.as_deref(), desc.as_deref(), caller)
            }
            BoardCommand::Post { board, title, tags, vi, content, body } => {
                let text = resolve_body_text(&body, content.as_deref());
                if vi {
                    self.board_post_vi(&board, &title, tags, &text, caller).await
                } else {
                    self.board_post(&board, &title, tags, &text, caller).await
                }
            }
            BoardCommand::Reply { topic, to, vi, content, body } => {
                let text = resolve_body_text(&body, content.as_deref());
                if vi {
                    self.board_reply_vi(&topic, to, &text, caller).await
                } else {
                    self.board_reply(&topic, to, &text, caller).await
                }
            }
            BoardCommand::Edit { post, vi, content, body } => {
                if vi {
                    self.board_edit_vi(&post, caller).await
                } else {
                    let text = resolve_body_text(&body, content.as_deref());
                    self.board_edit(&post, &text, caller).await
                }
            }
            BoardCommand::Resolve { topic } => {
                self.board_set_topic_status(&topic, TopicStatus::Resolved, "resolve", caller)
            }
            BoardCommand::Reopen { topic } => {
                self.board_set_topic_status(&topic, TopicStatus::Open, "reopen", caller)
            }
            BoardCommand::Archive { topic } => {
                self.board_set_topic_status(&topic, TopicStatus::Archived, "archive", caller)
            }
            BoardCommand::Pin { topic } => {
                self.board_set_topic_status(&topic, TopicStatus::Pinned, "pin", caller)
            }
            BoardCommand::Unpin { topic } => self.board_unpin(&topic, caller),
            BoardCommand::Lock { topic } => self.board_set_topic_locked(&topic, true, caller),
            BoardCommand::Unlock { topic } => self.board_set_topic_locked(&topic, false, caller),
            BoardCommand::Hide { post } => self.board_set_post_hidden(&post, true, caller),
            BoardCommand::Unhide { post } => self.board_set_post_hidden(&post, false, caller),
            BoardCommand::Promote { topic, to, title, summarize } => {
                self.board_promote(&topic, &to, title.as_deref(), summarize, caller).await
            }
        }
    }

    // ========================================================================
    // Read verbs
    // ========================================================================

    fn board_list(&self, character: Option<&str>, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let target_char = match character {
            Some(name) => match db.get_character_by_name(name) {
                Ok(Some(row)) => Some(row),
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj board list: no character named '{name}'; run `kj character list` to see who exists"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj board list: {e}")),
            },
            None => match db.get_character(caller.actor_id) {
                Ok(row) => row,
                Err(e) => return KjResult::Err(format!("kj board list: {e}")),
            },
        };

        let mut lines = Vec::new();
        let mut handles = Vec::new();

        if character.is_none()
            && let Some(ctx_id) = caller.context_id
            && let Ok(Some(ctx)) = db.get_context(ctx_id)
            && let Some(bid) = ctx.board_id
            && let Ok(Some(board)) = db.get_board(bid)
            && board.archived_at.is_none()
        {
            let desc = if board.description.is_empty() {
                "(context board)"
            } else {
                board.description.as_str()
            };
            lines.push(format!(".  [{}]  {desc}", board.write_policy));
            handles.push(serde_json::Value::String(".".to_string()));
        }

        if let Some(ch) = target_char {
            let boards = match db.list_character_boards(ch.principal_id, false) {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj board list: {e}")),
            };
            for board in boards {
                let slug = board.slug.as_deref().unwrap_or("-");
                let handle = format!("{}/{slug}", ch.name);
                if board.description.is_empty() {
                    lines.push(format!("{handle}  [{}]", board.write_policy));
                } else {
                    lines.push(format!("{handle}  [{}]  {}", board.write_policy, board.description));
                }
                handles.push(serde_json::Value::String(handle));
            }
        } else if lines.is_empty() {
            return KjResult::Err(format!(
                "kj board list: principal {} has no character; pass a character name or run `kj character create '<name>'`",
                caller.actor_id.short()
            ));
        }

        if lines.is_empty() {
            return KjResult::ok_with_data("(no boards)", serde_json::Value::Array(handles));
        }
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(handles))
    }

    fn board_topics(
        &self,
        board_ref: &str,
        status: Option<&str>,
        tag: Option<&str>,
        limit: usize,
        include_hidden: bool,
        caller: &KjCaller,
    ) -> KjResult {
        let mut topics = {
            let db = self.kernel_db().lock();
            let board = match self.resolve_board_read(board_ref, caller, &db) {
                Ok(ReadBoard::Found(b)) => b,
                Ok(ReadBoard::UncreatedContextBoard) => {
                    return KjResult::ok_with_data("(no topics)", serde_json::Value::Array(Vec::new()));
                }
                Err(e) => return KjResult::Err(format!("kj board topics: {e}")),
            };

            let (status_filter, exclude_archived) = match status {
                None => (None, true),
                Some("all") => (None, false),
                Some(s) => match TopicStatus::from_str(s) {
                    Ok(st) => (Some(st), false),
                    Err(_) => return KjResult::Err(format!("kj board topics: unknown status '{s}'")),
                },
            };

            let query_limit = if exclude_archived || !include_hidden {
                None
            } else {
                Some(limit)
            };
            let mut rows = match db.list_topics(board.board_id, status_filter, tag, query_limit) {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj board topics: {e}")),
            };
            if exclude_archived {
                rows.retain(|t| t.status != TopicStatus::Archived);
            }
            rows
        };

        if !include_hidden {
            topics.retain(|t| self.topic_has_visible_posts(t.document_id));
        }
        topics.truncate(limit);

        if topics.is_empty() {
            return KjResult::ok_with_data("(no topics)", serde_json::Value::Array(Vec::new()));
        }

        let lines: Vec<String> = topics
            .iter()
            .map(|t| format_topic_line(t, None))
            .collect();
        let ids: Vec<serde_json::Value> = topics
            .iter()
            .map(|t| serde_json::Value::String(t.document_id.short()))
            .collect();
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(ids))
    }

    fn board_read(
        &self,
        topic_ref: &str,
        tail: usize,
        after: Option<usize>,
        include_hidden: bool,
        caller: &KjCaller,
    ) -> KjResult {
        let topic = {
            let db = self.kernel_db().lock();
            match self.resolve_topic_with_archived(topic_ref, true, caller, &db) {
                Ok(t) => t,
                Err(e) => return KjResult::Err(format!("kj board read: {e}")),
            }
        };

        let blocks = match self.block_store().non_draft_snapshots(topic.document_id) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj board read: {e}")),
        };

        let db = self.kernel_db().lock();

        // Build 1-based post index across all blocks so hiding a post never
        // renumbers later posts and backrefs always resolve to stable numbers.
        struct IndexedPost<'a> {
            number: usize,
            author: String,
            snap: &'a BlockSnapshot,
        }
        let indexed: Vec<IndexedPost<'_>> = blocks
            .iter()
            .enumerate()
            .map(|(i, snap)| IndexedPost {
                number: i + 1,
                author: db.name_for(snap.author()),
                snap,
            })
            .collect();
        let by_id: HashMap<BlockId, &IndexedPost<'_>> =
            indexed.iter().map(|p| (p.snap.id, p)).collect();

        let visible: Vec<&IndexedPost<'_>> = indexed
            .iter()
            .filter(|p| include_hidden || !p.snap.excluded)
            .collect();

        let selected: Vec<&IndexedPost<'_>> = match after {
            Some(n) => visible.into_iter().filter(|p| p.number > n).collect(),
            None => {
                let start = visible.len().saturating_sub(tail);
                visible[start..].to_vec()
            }
        };

        let mut out = String::new();
        let lock_tag = if topic.locked { " [locked]" } else { "" };
        let tags_str = if topic.tags.is_empty() {
            String::new()
        } else {
            format!(" ({})", topic.tags.join(", "))
        };
        out.push_str(&format!(
            "# {} [{}]{}{}  {}\n",
            topic.title,
            topic.status,
            lock_tag,
            tags_str,
            topic.document_id.short(),
        ));

        let mut posts_json = Vec::with_capacity(selected.len());
        if selected.is_empty() {
            out.push_str("(no matching posts)");
        } else {
            for post in &selected {
                out.push('\n');
                let age = super::format::format_age_compact(post.snap.created_at as i64);
                let hidden_tag = if post.snap.excluded { " [hidden]" } else { "" };
                out.push_str(&format!("#{} @{} ({age}){hidden_tag}\n", post.number, post.author));

                let mut parent_number = None;
                if let Some(pid) = post.snap.parent_id
                    && let Some(parent) = by_id.get(&pid)
                {
                    parent_number = Some(parent.number);
                    if parent.snap.excluded && !include_hidden {
                        out.push_str(&format!("[re #{} (hidden)]\n", parent.number));
                    } else {
                        let preview = backref_preview(&parent.snap.content);
                        out.push_str(&format!(
                            "[re #{} @{}: \"{preview}\"]\n",
                            parent.number, parent.author
                        ));
                    }
                }
                out.push_str(&post.snap.content);
                if !post.snap.content.ends_with('\n') {
                    out.push('\n');
                }

                posts_json.push(serde_json::json!({
                    "number": post.number,
                    "block_id": post.snap.id.to_key(),
                    "author": post.author,
                    "author_id": post.snap.author().to_hex(),
                    "created_at": post.snap.created_at,
                    "excluded": post.snap.excluded,
                    "parent_number": parent_number,
                    "content": post.snap.content,
                }));
            }
        }

        let data = serde_json::json!({
            "document_id": topic.document_id.to_hex(),
            "board_id": topic.board_id.to_hex(),
            "slug": topic.slug,
            "title": topic.title,
            "status": topic.status.as_str(),
            "locked": topic.locked,
            "tags": topic.tags,
            "post_count": topic.post_count,
            "posts": posts_json,
        });
        KjResult::ok_typed_with_data(out.trim_end().to_string(), ContentType::Markdown, data)
    }

    fn board_recent(
        &self,
        board_ref: Option<&str>,
        since: &str,
        limit: usize,
        include_hidden: bool,
        caller: &KjCaller,
    ) -> KjResult {
        let duration_ms = match parse_duration_ms(since) {
            Ok(ms) => ms,
            Err(e) => return KjResult::Err(format!("kj board recent: {e}")),
        };
        let since_ms = (now_millis() as i64).saturating_sub(duration_ms);

        let mut topics = {
            let db = self.kernel_db().lock();
            let board_filter = match board_ref {
                Some(r) => match self.resolve_board_read(r, caller, &db) {
                    Ok(ReadBoard::Found(b)) => Some(b.board_id),
                    Ok(ReadBoard::UncreatedContextBoard) => {
                        return KjResult::ok_with_data("(no recent topics)", serde_json::Value::Array(Vec::new()));
                    }
                    Err(e) => return KjResult::Err(format!("kj board recent: {e}")),
                },
                None => None,
            };

            let query_limit = if include_hidden { Some(limit) } else { None };
            match db.list_recent_topics(board_filter, Some(since_ms), query_limit) {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj board recent: {e}")),
            }
        };

        if !include_hidden {
            topics.retain(|t| self.topic_has_visible_posts(t.document_id));
        }
        topics.truncate(limit);

        if topics.is_empty() {
            return KjResult::ok_with_data("(no recent topics)", serde_json::Value::Array(Vec::new()));
        }

        let db = self.kernel_db().lock();
        let mut board_labels: HashMap<BoardId, String> = HashMap::new();
        let mut lines = Vec::with_capacity(topics.len());
        let mut ids = Vec::with_capacity(topics.len());
        for t in &topics {
            let label = board_labels
                .entry(t.board_id)
                .or_insert_with(|| format_board_label(&db, t.board_id));
            lines.push(format_topic_line(t, Some(label.as_str())));
            ids.push(serde_json::Value::String(t.document_id.short()));
        }
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(ids))
    }

    async fn board_search(
        &self,
        query: &str,
        board_ref: Option<&str>,
        semantic: bool,
        limit: usize,
        caller: &KjCaller,
    ) -> KjResult {
        let board_filter = {
            let db = self.kernel_db().lock();
            match board_ref {
                Some(r) => match self.resolve_board_read(r, caller, &db) {
                    Ok(ReadBoard::Found(b)) => Some(b.board_id),
                    Ok(ReadBoard::UncreatedContextBoard) => {
                        return KjResult::ok_with_data("(no matches)", serde_json::Value::Array(Vec::new()));
                    }
                    Err(e) => return KjResult::Err(format!("kj board search: {e}")),
                },
                None => None,
            }
        };

        if semantic {
            return self.board_search_semantic(query, board_filter, limit).await;
        }
        self.board_search_regex(query, board_filter, limit)
    }

    async fn board_search_semantic(
        &self,
        query: &str,
        board_filter: Option<BoardId>,
        limit: usize,
    ) -> KjResult {
        let Some(index) = self.semantic_index() else {
            return KjResult::Err(
                "kj board search: semantic index is not configured; run `kj embedding show` to inspect configuration, or pass --regex"
                    .to_string(),
            );
        };

        let topic_ids: HashSet<ContextId> = {
            let db = self.kernel_db().lock();
            match db.list_topic_ids(board_filter) {
                Ok(ids) => ids.into_iter().collect(),
                Err(e) => return KjResult::Err(format!("kj board search: {e}")),
            }
        };

        if topic_ids.is_empty() {
            return KjResult::ok_with_data("(no matches)", serde_json::Value::Array(Vec::new()));
        }

        let hits = match index.search_in(query, &topic_ids, limit).await {
            Ok(h) => h,
            Err(e) => return KjResult::Err(format!("kj board search: semantic search failed: {e}")),
        };

        if hits.is_empty() {
            return KjResult::ok_with_data("(no matches)", serde_json::Value::Array(Vec::new()));
        }

        let db = self.kernel_db().lock();
        let mut lines = Vec::with_capacity(hits.len());
        let mut data = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(topic) = db.get_topic(hit.context_id).ok().flatten() else {
                continue;
            };
            let board_label = format_board_label(&db, topic.board_id);
            lines.push(format!(
                "{:.2}  {}  {}/{}  {}",
                hit.score,
                topic.document_id.short(),
                board_label,
                topic.slug,
                topic.title,
            ));
            data.push(serde_json::Value::String(topic.document_id.short()));
        }

        if lines.is_empty() {
            return KjResult::ok_with_data("(no matches)", serde_json::Value::Array(Vec::new()));
        }
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(data))
    }

    fn board_search_regex(
        &self,
        query: &str,
        board_filter: Option<BoardId>,
        limit: usize,
    ) -> KjResult {
        let regex = match Regex::new(query) {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj board search: invalid regex: {e}")),
        };

        // Snapshot topic metadata and board labels under a short DB lock so we
        // never hold the KernelDb mutex across the full-corpus block scan.
        let candidates: Vec<(BoardTopicRow, String)> = {
            let db = self.kernel_db().lock();
            let topic_ids = match db.list_topic_ids(board_filter) {
                Ok(ids) => ids,
                Err(e) => return KjResult::Err(format!("kj board search: {e}")),
            };
            let mut board_labels: HashMap<BoardId, String> = HashMap::new();
            let mut out = Vec::with_capacity(topic_ids.len());
            for doc_id in topic_ids {
                let Some(topic) = db.get_topic(doc_id).ok().flatten() else {
                    continue;
                };
                let label = board_labels
                    .entry(topic.board_id)
                    .or_insert_with(|| format_board_label(&db, topic.board_id))
                    .clone();
                out.push((topic, label));
            }
            out
        };

        struct RegexHit {
            short_id: String,
            board_label: String,
            slug: String,
            post_num: usize,
            author_id: PrincipalId,
            line: String,
        }
        let mut hits: Vec<RegexHit> = Vec::new();

        'outer: for (topic, board_label) in candidates {
            let Ok(snaps) = self.block_store().non_draft_snapshots(topic.document_id) else {
                continue;
            };
            let short_id = topic.document_id.short();

            for (idx, snap) in snaps.iter().enumerate() {
                if snap.excluded {
                    continue;
                }
                let post_num = idx + 1;
                for line in snap.content.lines() {
                    if regex.is_match(line) {
                        hits.push(RegexHit {
                            short_id: short_id.clone(),
                            board_label: board_label.clone(),
                            slug: topic.slug.clone(),
                            post_num,
                            author_id: snap.author(),
                            line: line.trim().to_string(),
                        });
                        if hits.len() >= limit {
                            break 'outer;
                        }
                    }
                }
            }
        }

        if hits.is_empty() {
            return KjResult::ok_with_data("(no matches)", serde_json::Value::Array(Vec::new()));
        }

        let author_names: HashMap<PrincipalId, String> = {
            let db = self.kernel_db().lock();
            let mut names = HashMap::new();
            for hit in &hits {
                names
                    .entry(hit.author_id)
                    .or_insert_with(|| db.name_for(hit.author_id));
            }
            names
        };

        let mut lines = Vec::with_capacity(hits.len());
        let mut data = Vec::with_capacity(hits.len());
        for hit in hits {
            let author = author_names
                .get(&hit.author_id)
                .map(String::as_str)
                .unwrap_or("unknown");
            lines.push(format!(
                "{}  {}/{}#{}  @{}: {}",
                hit.short_id, hit.board_label, hit.slug, hit.post_num, author, hit.line,
            ));
            data.push(serde_json::Value::String(format!(
                "{}#{}",
                hit.short_id, hit.post_num
            )));
        }
        KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(data))
    }

    // ========================================================================
    // Write verbs
    // ========================================================================

    fn board_create(
        &self,
        slug: &str,
        for_character: Option<&str>,
        policy_str: &str,
        desc: &str,
        caller: &KjCaller,
    ) -> KjResult {
        if let Err(e) = validate_board_slug(slug) {
            return KjResult::Err(format!("kj board create: {e}"));
        }
        let policy = match BoardWritePolicy::from_str(policy_str) {
            Ok(p) => p,
            Err(_) => {
                return KjResult::Err(format!(
                    "kj board create: unknown policy '{policy_str}'; expected open, curated, or locked"
                ));
            }
        };

        let db = self.kernel_db().lock();
        let owner = match for_character {
            Some(name) => match db.get_character_by_name(name) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj board create: no character named '{name}'; run `kj character list` to see who exists"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj board create: {e}")),
            },
            None => match self.resolve_board_caller_character(caller, "create", &db) {
                Ok(row) => row,
                Err(e) => return KjResult::Err(e),
            },
        };

        if owner.retired_at.is_some() {
            return KjResult::Err(format!(
                "kj board create: character '{}' is retired and cannot receive new boards",
                owner.name
            ));
        }

        if owner.principal_id != caller.actor_id {
            match self.can_moderate_character(owner.principal_id, caller, &db) {
                Ok(true) => {}
                Ok(false) => {
                    return KjResult::Err(format!(
                        "kj board create: creating a board for '{}' requires senpai, house, or operator authority",
                        owner.name
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj board create: {e}")),
            }
        }

        let row = BoardRow {
            board_id: BoardId::new(),
            owner_character: Some(owner.principal_id),
            slug: Some(slug.to_string()),
            description: desc.to_string(),
            write_policy: policy,
            created_by: caller.actor_id,
            created_at: now_millis() as i64,
            archived_at: None,
        };
        if let Err(e) = db.insert_board(&row) {
            return KjResult::Err(format!("kj board create: {e}"));
        }

        let handle = format!("{}/{slug}", owner.name);
        KjResult::ok_with_data(
            format!("created board '{handle}' [{}]", row.write_policy),
            serde_json::json!({
                "board_id": row.board_id.to_hex(),
                "handle": handle,
                "owner": owner.name,
                "slug": slug,
                "write_policy": row.write_policy.as_str(),
                "description": row.description,
            }),
        )
    }

    fn board_set(
        &self,
        board_ref: &str,
        policy_str: Option<&str>,
        desc: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        if policy_str.is_none() && desc.is_none() {
            return KjResult::Err(
                "kj board set: pass --policy <open|curated|locked> and/or --desc '<text>'"
                    .to_string(),
            );
        }
        let policy = match policy_str {
            Some(s) => match BoardWritePolicy::from_str(s) {
                Ok(p) => Some(p),
                Err(_) => {
                    return KjResult::Err(format!(
                        "kj board set: unknown policy '{s}'; expected open, curated, or locked"
                    ));
                }
            },
            None => None,
        };

        let db = self.kernel_db().lock();
        let board = match self.resolve_board_write(board_ref, caller, &db) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj board set: {e}")),
        };
        if let Err(e) = self.require_board_moderator(&board, caller, "set", &db) {
            return KjResult::Err(e);
        }
        if let Err(e) = db.update_board(board.board_id, policy, desc) {
            return KjResult::Err(format!("kj board set: {e}"));
        }
        let updated = db
            .get_board(board.board_id)
            .ok()
            .flatten()
            .unwrap_or(board);
        let label = format_board_label(&db, updated.board_id);
        KjResult::ok_with_data(
            format!("updated board '{label}' [{}]", updated.write_policy),
            serde_json::json!({
                "board_id": updated.board_id.to_hex(),
                "write_policy": updated.write_policy.as_str(),
                "description": updated.description,
            }),
        )
    }

    pub(crate) async fn board_post(
        &self,
        board_ref: &str,
        title: &str,
        tags: Vec<String>,
        text: &str,
        caller: &KjCaller,
    ) -> KjResult {
        let title = title.trim();
        if title.is_empty() {
            return KjResult::Err("kj board post: --title must not be empty".to_string());
        }
        if text.trim().is_empty() {
            return KjResult::Err(
                "kj board post: post body must not be empty; pass body text or pipe stdin"
                    .to_string(),
            );
        }
        if text.len() > MAX_POST_BYTES {
            return KjResult::Err(format!(
                "kj board post: post body is {} bytes, exceeding the {} KiB limit ({} bytes)",
                text.len(),
                MAX_POST_BYTES / 1024,
                MAX_POST_BYTES,
            ));
        }
        self.create_board_topic_post(board_ref, title, tags, text, None, "post", caller)
            .await
    }

    async fn board_promote(
        &self,
        source_ref: &str,
        to_board: &str,
        title_override: Option<&str>,
        summarize: bool,
        caller: &KjCaller,
    ) -> KjResult {
        let (source_topic, source_board_label) = {
            let db = self.kernel_db().lock();
            let topic = match self.resolve_topic(source_ref, caller, &db) {
                Ok(t) => t,
                Err(e) => return KjResult::Err(format!("kj board promote: {e}")),
            };
            let label = format_board_label(&db, topic.board_id);
            (topic, label)
        };

        let target_title = title_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&source_topic.title)
            .to_string();
        if let Some(raw) = title_override
            && raw.trim().is_empty()
        {
            return KjResult::Err("kj board promote: --title must not be empty".to_string());
        }

        let all_blocks = match self.block_store().non_draft_snapshots(source_topic.document_id) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj board promote: {e}")),
        };
        let visible_indexed: Vec<(usize, BlockSnapshot)> = all_blocks
            .into_iter()
            .enumerate()
            .filter(|(_, b)| !b.excluded)
            .map(|(i, b)| (i + 1, b))
            .collect();
        if visible_indexed.is_empty() {
            return KjResult::Err(format!(
                "kj board promote: source topic '{}' has no visible posts to promote",
                source_topic.slug
            ));
        }

        let provenance = format!(
            "_Promoted from `{source_board_label}/{}` ({})_\n\n",
            source_topic.slug,
            source_topic.document_id.short()
        );

        let body = if summarize {
            let Some(caller_ctx) = caller.context_id else {
                return KjResult::Err(
                    "kj board promote --summarize: requires an active context with a configured `distill` slot"
                        .to_string(),
                );
            };
            let visible_blocks: Vec<BlockSnapshot> =
                visible_indexed.into_iter().map(|(_, b)| b).collect();
            let summary = match self
                .summarize_board_topic(caller_ctx, visible_blocks, &source_topic.title)
                .await
            {
                Ok(s) => s,
                Err(e) => return KjResult::Err(format!("kj board promote --summarize: {e}")),
            };
            format!("{provenance}{summary}")
        } else {
            let db = self.kernel_db().lock();
            let mut out = provenance;
            for (idx, (post_num, snap)) in visible_indexed.iter().enumerate() {
                if idx > 0 {
                    out.push('\n');
                }
                let author = db.name_for(snap.author());
                out.push_str(&format!("### #{post_num} @{author}\n\n"));
                out.push_str(&snap.content);
                if !snap.content.ends_with('\n') {
                    out.push('\n');
                }
            }
            out.trim_end().to_string()
        };

        if body.len() > MAX_POST_BYTES {
            return KjResult::Err(format!(
                "kj board promote: promoted body is {} bytes, exceeding the {} KiB limit ({} bytes); pass --summarize to distill it",
                body.len(),
                MAX_POST_BYTES / 1024,
                MAX_POST_BYTES,
            ));
        }

        self.create_board_topic_post(
            to_board,
            &target_title,
            source_topic.tags,
            &body,
            Some(source_topic.document_id),
            "promote",
            caller,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_board_topic_post(
        &self,
        board_ref: &str,
        title: &str,
        tags: Vec<String>,
        text: &str,
        promoted_from: Option<ContextId>,
        verb: &str,
        caller: &KjCaller,
    ) -> KjResult {
        let gate_config = match crate::kj::gate_policy::load_config(
            self.kernel().vfs(),
            self.kernel_db(),
        )
        .await
        {
            Ok(cfg) => cfg,
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };

        let (board, slug, board_label, decision, is_privileged_or_root) = {
            let db = self.kernel_db().lock();
            let board = match self.resolve_board_write(board_ref, caller, &db) {
                Ok(b) => b,
                Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
            };
            if board.archived_at.is_some() {
                return KjResult::Err(format!(
                    "kj board {verb}: board '{}' is archived and read-only",
                    format_board_label(&db, board.board_id)
                ));
            }
            let decision = match self.publish_or_hold_post(&board, caller, verb, &gate_config, &db) {
                Ok(d) => d,
                Err(e) => return KjResult::Err(e),
            };
            let slug = match unique_topic_slug(&db, board.board_id, title) {
                Ok(s) => s,
                Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
            };
            let label = format_board_label(&db, board.board_id);
            let is_priv_or_root = caller.privileged
                || db
                    .get_character(caller.actor_id)
                    .ok()
                    .flatten()
                    .is_some_and(|c| c.is_live_root());
            (board, slug, label, decision, is_priv_or_root)
        };

        let now = now_millis() as i64;
        let should_rate_limit = match decision {
            WriteDecision::PublishDirect => board.write_policy == BoardWritePolicy::Open,
            WriteDecision::HoldForReview { .. } => !is_privileged_or_root,
        };
        if should_rate_limit
            && let Err(e) = self
                .board_rate_limiter()
                .check_and_record(board.board_id, caller.actor_id, now)
        {
            return KjResult::Err(format!("kj board {verb}: {e}"));
        }

        let doc_id = ContextId::new();
        let block_id = match decision {
            WriteDecision::PublishDirect => self.block_store().create_document_with_block(
                doc_id,
                caller.actor_id,
                DocKind::Topic,
                None,
                Role::User,
                BlockKind::Text,
                text.to_string(),
                Status::Done,
                ContentType::Markdown,
            ),
            WriteDecision::HoldForReview { .. } => {
                self.block_store().create_document_with_held_block(
                    doc_id,
                    caller.actor_id,
                    DocKind::Topic,
                    None,
                    Role::User,
                    BlockKind::Text,
                    text.to_string(),
                    ContentType::Markdown,
                )
            }
        };
        let block_id = match block_id {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };

        let topic_row = BoardTopicRow {
            document_id: doc_id,
            board_id: board.board_id,
            slug: slug.clone(),
            title: title.to_string(),
            status: TopicStatus::Open,
            locked: false,
            tags,
            promoted_from,
            created_by: caller.actor_id,
            created_at: now,
            last_post_at: now,
            post_count: 1,
        };
        if let Err(e) = self.kernel_db().lock().insert_topic(&topic_row) {
            return KjResult::Err(format!("kj board {verb}: {e}"));
        }

        if let WriteDecision::HoldForReview { reviewer } = decision {
            let ask_id = match self.open_board_post_ask(
                &board,
                &board_label,
                doc_id,
                &slug,
                title,
                block_id,
                text,
                verb,
                caller,
                reviewer,
            ) {
                Ok(id) => id,
                Err(e) => {
                    if let Err(dismiss) =
                        self.block_store().settle_held_board_post(doc_id, &block_id, false)
                    {
                        tracing::error!(
                            block = %block_id.to_key(),
                            "could not dismiss held board post with no ask: {dismiss}"
                        );
                    }
                    return KjResult::Err(format!(
                        "kj board {verb}: could not hold post for review: {e}"
                    ));
                }
            };
            return KjResult::ok_with_data(
                format!(
                    "held {verb} on '{board_label}/{slug}' ({}) #1 for review (ask '{ask_id}'); release with `kj ledger allow '{ask_id}'`",
                    doc_id.short()
                ),
                serde_json::json!({
                    "held": true,
                    "ask_id": ask_id,
                    "document_id": doc_id.to_hex(),
                    "short_id": doc_id.short(),
                    "board_id": board.board_id.to_hex(),
                    "slug": slug,
                    "title": title,
                    "block_id": block_id.to_key(),
                    "post_number": 1,
                    "promoted_from": promoted_from.map(|id| id.to_hex()),
                }),
            );
        }

        let verb_past = if verb == "promote" { "promoted" } else { "posted" };
        KjResult::ok_with_data(
            format!(
                "{verb_past} topic '{board_label}/{slug}' ({}) #1",
                doc_id.short()
            ),
            serde_json::json!({
                "held": false,
                "document_id": doc_id.to_hex(),
                "short_id": doc_id.short(),
                "board_id": board.board_id.to_hex(),
                "slug": slug,
                "title": title,
                "block_id": block_id.to_key(),
                "post_number": 1,
                "promoted_from": promoted_from.map(|id| id.to_hex()),
            }),
        )
    }

    pub(crate) async fn board_reply(
        &self,
        topic_ref: &str,
        to: Option<usize>,
        text: &str,
        caller: &KjCaller,
    ) -> KjResult {
        if text.trim().is_empty() {
            return KjResult::Err(
                "kj board reply: reply body must not be empty; pass body text or pipe stdin"
                    .to_string(),
            );
        }
        if text.len() > MAX_POST_BYTES {
            return KjResult::Err(format!(
                "kj board reply: reply body is {} bytes, exceeding the {} KiB limit ({} bytes)",
                text.len(),
                MAX_POST_BYTES / 1024,
                MAX_POST_BYTES,
            ));
        }

        let gate_config = match crate::kj::gate_policy::load_config(
            self.kernel().vfs(),
            self.kernel_db(),
        )
        .await
        {
            Ok(cfg) => cfg,
            Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
        };

        let (board, topic, board_label, decision, is_privileged_or_root) = {
            let db = self.kernel_db().lock();
            let topic = match self.resolve_topic(topic_ref, caller, &db) {
                Ok(t) => t,
                Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
            };
            let board = match db.get_board(topic.board_id) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj board reply: board {} for topic '{}' no longer exists",
                        topic.board_id.short(),
                        topic.slug
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
            };
            if board.archived_at.is_some() {
                return KjResult::Err(format!(
                    "kj board reply: board '{}' is archived and read-only",
                    format_board_label(&db, board.board_id)
                ));
            }
            if topic.status == TopicStatus::Archived {
                return KjResult::Err(format!(
                    "kj board reply: topic '{}' is archived; reopen it first with `kj board reopen '{}'`",
                    topic.slug,
                    topic.document_id.short()
                ));
            }
            if topic.locked {
                return KjResult::Err(format!(
                    "kj board reply: topic '{}' is locked and accepts no new replies",
                    topic.slug
                ));
            }
            let decision = match self.publish_or_hold_post(&board, caller, "reply", &gate_config, &db) {
                Ok(d) => d,
                Err(e) => return KjResult::Err(e),
            };
            let label = format_board_label(&db, board.board_id);
            let is_priv_or_root = caller.privileged
                || db
                    .get_character(caller.actor_id)
                    .ok()
                    .flatten()
                    .is_some_and(|c| c.is_live_root());
            (board, topic, label, decision, is_priv_or_root)
        };

        let existing = match self.block_store().non_draft_snapshots(topic.document_id) {
            Ok(snaps) => snaps,
            Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
        };
        let parent_id = match to {
            Some(0) => {
                return KjResult::Err(
                    "kj board reply: --to post numbers are 1-based (start at 1)".to_string(),
                );
            }
            Some(n) => match existing.get(n - 1) {
                Some(snap) => Some(snap.id),
                None => {
                    return KjResult::Err(format!(
                        "kj board reply: post #{n} does not exist in topic '{}' (has {} posts)",
                        topic.slug,
                        existing.len()
                    ));
                }
            },
            None => None,
        };

        let now = now_millis() as i64;
        let should_rate_limit = match decision {
            WriteDecision::PublishDirect => board.write_policy == BoardWritePolicy::Open,
            WriteDecision::HoldForReview { .. } => !is_privileged_or_root,
        };
        if should_rate_limit
            && let Err(e) = self
                .board_rate_limiter()
                .check_and_record(board.board_id, caller.actor_id, now)
        {
            return KjResult::Err(format!("kj board reply: {e}"));
        }

        let block_id = match decision {
            WriteDecision::PublishDirect => self.block_store().insert_block_as(
                topic.document_id,
                parent_id.as_ref(),
                None,
                Role::User,
                BlockKind::Text,
                text.to_string(),
                Status::Done,
                ContentType::Markdown,
                Some(caller.actor_id),
            ),
            WriteDecision::HoldForReview { .. } => self.block_store().insert_held_block_as(
                topic.document_id,
                parent_id.as_ref(),
                None,
                Role::User,
                BlockKind::Text,
                text.to_string(),
                ContentType::Markdown,
                caller.actor_id,
            ),
        };
        let block_id = match block_id {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
        };

        let post_num = existing.len() + 1;

        if let WriteDecision::HoldForReview { reviewer } = decision {
            let ask_id = match self.open_board_post_ask(
                &board,
                &board_label,
                topic.document_id,
                &topic.slug,
                &topic.title,
                block_id,
                text,
                "reply",
                caller,
                reviewer,
            ) {
                Ok(id) => id,
                Err(e) => {
                    if let Err(dismiss) = self
                        .block_store()
                        .settle_held_board_post(topic.document_id, &block_id, false)
                    {
                        tracing::error!(
                            block = %block_id.to_key(),
                            "could not dismiss held board reply with no ask: {dismiss}"
                        );
                    }
                    return KjResult::Err(format!(
                        "kj board reply: could not hold reply for review: {e}"
                    ));
                }
            };
            return KjResult::ok_with_data(
                format!(
                    "held reply on '{board_label}/{}' ({}) #{post_num} for review (ask '{ask_id}'); release with `kj ledger allow '{ask_id}'`",
                    topic.slug,
                    topic.document_id.short()
                ),
                serde_json::json!({
                    "held": true,
                    "ask_id": ask_id,
                    "document_id": topic.document_id.to_hex(),
                    "short_id": topic.document_id.short(),
                    "slug": topic.slug,
                    "block_id": block_id.to_key(),
                    "post_number": post_num,
                    "parent_number": to,
                }),
            );
        }

        if let Err(e) = self.kernel_db().lock().bump_topic_post_stats(topic.document_id, now) {
            return KjResult::Err(format!("kj board reply: {e}"));
        }

        KjResult::ok_with_data(
            format!(
                "replied to '{board_label}/{}' ({}) #{post_num}",
                topic.slug,
                topic.document_id.short()
            ),
            serde_json::json!({
                "held": false,
                "document_id": topic.document_id.to_hex(),
                "short_id": topic.document_id.short(),
                "slug": topic.slug,
                "block_id": block_id.to_key(),
                "post_number": post_num,
                "parent_number": to,
            }),
        )
    }

    /// Resolve a post ref and verify the caller is allowed to edit it.
    /// Returns `(board_label, topic, post_num, target_snapshot)`.
    async fn resolve_editable_post(
        &self,
        post_ref: &str,
        caller: &KjCaller,
    ) -> Result<(String, BoardTopicRow, usize, BlockSnapshot), String> {
        let (topic_part, post_num) = parse_post_ref(post_ref)?;
        let gate_config = crate::kj::gate_policy::load_config(
            self.kernel().vfs(),
            self.kernel_db(),
        )
        .await
        .map_err(|e| e.to_string())?;

        let (board, topic, board_label) = {
            let db = self.kernel_db().lock();
            let topic = self.resolve_topic(topic_part, caller, &db)?;
            let board = db
                .get_board(topic.board_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("board {} not found", topic.board_id.short()))?;
            if board.archived_at.is_some() {
                return Err(format!(
                    "board '{}' is archived and read-only",
                    format_board_label(&db, board.board_id)
                ));
            }
            if topic.status == TopicStatus::Archived {
                return Err(format!(
                    "topic '{}' is archived; reopen it first with `kj board reopen '{}'`",
                    topic.slug,
                    topic.document_id.short()
                ));
            }
            if topic.locked {
                return Err(format!(
                    "topic '{}' is locked; unlock it first with `kj board unlock '{}'`",
                    topic.slug,
                    topic.document_id.short()
                ));
            }
            let label = format_board_label(&db, board.board_id);
            (board, topic, label)
        };

        let snaps = self
            .block_store()
            .non_draft_snapshots(topic.document_id)
            .map_err(|e| e.to_string())?;
        let target = snaps.get(post_num - 1).cloned().ok_or_else(|| {
            format!(
                "post #{post_num} does not exist in topic '{}' (has {} posts)",
                topic.slug,
                snaps.len()
            )
        })?;

        if target.status == Status::Pending && target.excluded {
            return Err(format!(
                "post #{post_num} in topic '{}' is held pending review and cannot be edited",
                topic.slug
            ));
        }

        {
            let db = self.kernel_db().lock();
            match board.write_policy {
                BoardWritePolicy::Locked => {
                    if !self.caller_is_root_or_house(caller, &db).unwrap_or(false) {
                        return Err(format!(
                            "board '{board_label}' is locked; only root or house can edit posts"
                        ));
                    }
                }
                BoardWritePolicy::Curated => {
                    let is_mod = self.require_board_moderator(&board, caller, "edit", &db).is_ok();
                    let can_direct_author = target.author() == caller.actor_id
                        && matches!(
                            self.publish_or_hold_post(&board, caller, "edit", &gate_config, &db),
                            Ok(WriteDecision::PublishDirect)
                        );
                    if !is_mod && !can_direct_author {
                        return Err(format!(
                            "editing posts on curated board '{board_label}' requires board owner, senpai, root, or direct-publish authority"
                        ));
                    }
                }
                BoardWritePolicy::Open => {
                    let is_author = target.author() == caller.actor_id;
                    if !is_author
                        && let Err(e) = self.require_board_moderator(&board, caller, "edit", &db)
                    {
                        return Err(e.trim_start_matches("kj board edit: ").to_string());
                    }
                }
            }
        }

        Ok((board_label, topic, post_num, target))
    }

    pub(crate) async fn board_edit(
        &self,
        post_ref: &str,
        text: &str,
        caller: &KjCaller,
    ) -> KjResult {
        if text.trim().is_empty() {
            return KjResult::Err(
                "kj board edit: replacement body must not be empty; pass body text or pipe stdin"
                    .to_string(),
            );
        }
        if text.len() > MAX_POST_BYTES {
            return KjResult::Err(format!(
                "kj board edit: replacement body is {} bytes, exceeding the {} KiB limit ({} bytes)",
                text.len(),
                MAX_POST_BYTES / 1024,
                MAX_POST_BYTES,
            ));
        }

        let (board_label, topic, post_num, target) = match self.resolve_editable_post(post_ref, caller).await {
            Ok(resolved) => resolved,
            Err(e) => return KjResult::Err(format!("kj board edit: {e}")),
        };

        let old_chars = target.content.chars().count();
        if let Err(e) = self.block_store().edit_text_as(
            topic.document_id,
            &target.id,
            0,
            text,
            old_chars,
            Some(caller.actor_id),
        ) {
            return KjResult::Err(format!("kj board edit: {e}"));
        }

        KjResult::ok_with_data(
            format!(
                "edited post #{post_num} in '{board_label}/{}' ({})",
                topic.slug,
                topic.document_id.short()
            ),
            serde_json::json!({
                "document_id": topic.document_id.to_hex(),
                "short_id": topic.document_id.short(),
                "slug": topic.slug,
                "post_number": post_num,
                "block_id": target.id.to_key(),
            }),
        )
    }

    fn editor_opener_for(caller: &KjCaller) -> Option<crate::editor::EditorOpener> {
        caller.context_id.map(|context_id| crate::editor::EditorOpener {
            principal: caller.principal_id,
            performer: caller.actor_id,
            reviewer: caller.reviewer_id,
            context_id,
            session_id: caller.session_id,
        })
    }

    async fn board_edit_vi(&self, post_ref: &str, caller: &KjCaller) -> KjResult {
        let (board_label, topic, post_num, target) = match self.resolve_editable_post(post_ref, caller).await {
            Ok(resolved) => resolved,
            Err(e) => return KjResult::Err(format!("kj board edit: {e}")),
        };
        let display_path = format!("board://{board_label}/{}#{post_num}", topic.slug);
        let canonical_ref = format!("{}#{post_num}", topic.document_id.short());
        let opener = Self::editor_opener_for(caller);
        match self
            .kernel()
            .editor_open_board_signaled(
                &display_path,
                &target.content,
                crate::editor::BoardEditorTarget::EditPost { post_ref: canonical_ref },
                caller.clone(),
                opener,
            )
            .await
        {
            Ok((id, st)) => KjResult::ok_with_data(
                format!("opened editor session {} on {}", id.as_u64(), st.path),
                st.to_json(id),
            ),
            Err(e) => KjResult::Err(format!("kj board edit --vi: {e}")),
        }
    }

    async fn board_reply_vi(
        &self,
        topic_ref: &str,
        to: Option<usize>,
        initial_text: &str,
        caller: &KjCaller,
    ) -> KjResult {
        let (board_label, topic) = {
            let db = self.kernel_db().lock();
            let topic = match self.resolve_topic(topic_ref, caller, &db) {
                Ok(t) => t,
                Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
            };
            let board = match db.get_board(topic.board_id) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj board reply: board {} not found",
                        topic.board_id.short()
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj board reply: {e}")),
            };
            if board.archived_at.is_some() {
                return KjResult::Err(format!(
                    "kj board reply: board '{}' is archived and read-only",
                    format_board_label(&db, board.board_id)
                ));
            }
            if topic.status == TopicStatus::Archived {
                return KjResult::Err(format!(
                    "kj board reply: topic '{}' is archived; reopen it first with `kj board reopen '{}'`",
                    topic.slug,
                    topic.document_id.short()
                ));
            }
            if topic.locked {
                return KjResult::Err(format!(
                    "kj board reply: topic '{}' is locked and accepts no new replies",
                    topic.slug
                ));
            }
            (format_board_label(&db, board.board_id), topic)
        };

        if let Some(n) = to {
            if n == 0 {
                return KjResult::Err(
                    "kj board reply: --to post numbers are 1-based (start at 1)".to_string(),
                );
            }
            let count = self
                .block_store()
                .non_draft_snapshots(topic.document_id)
                .map(|s| s.len())
                .unwrap_or(0);
            if n > count {
                return KjResult::Err(format!(
                    "kj board reply: post #{n} does not exist in topic '{}' (has {count} posts)",
                    topic.slug
                ));
            }
        }

        let display_path = match to {
            Some(n) => format!("board://{board_label}/{}#re-{n}", topic.slug),
            None => format!("board://{board_label}/{}#new", topic.slug),
        };
        let opener = Self::editor_opener_for(caller);
        match self
            .kernel()
            .editor_open_board_signaled(
                &display_path,
                initial_text,
                crate::editor::BoardEditorTarget::NewReply {
                    topic_ref: topic.document_id.short(),
                    to,
                },
                caller.clone(),
                opener,
            )
            .await
        {
            Ok((id, st)) => KjResult::ok_with_data(
                format!("opened editor session {} on {}", id.as_u64(), st.path),
                st.to_json(id),
            ),
            Err(e) => KjResult::Err(format!("kj board reply --vi: {e}")),
        }
    }

    async fn board_post_vi(
        &self,
        board_ref: &str,
        title: &str,
        tags: Vec<String>,
        initial_text: &str,
        caller: &KjCaller,
    ) -> KjResult {
        let title = title.trim();
        if title.is_empty() {
            return KjResult::Err("kj board post: --title must not be empty".to_string());
        }
        let board_label = {
            let db = self.kernel_db().lock();
            let board = match self.resolve_board_read(board_ref, caller, &db) {
                Ok(ReadBoard::Found(b)) => {
                    if b.archived_at.is_some() {
                        return KjResult::Err(format!(
                            "kj board post: board '{}' is archived and read-only",
                            format_board_label(&db, b.board_id)
                        ));
                    }
                    Some(b)
                }
                Ok(ReadBoard::UncreatedContextBoard) => None,
                Err(e) => return KjResult::Err(format!("kj board post: {e}")),
            };
            board
                .map(|b| format_board_label(&db, b.board_id))
                .unwrap_or_else(|| board_ref.to_string())
        };
        let slug_preview = slugify_title(title);
        let display_path = format!("board://{board_label}/{slug_preview}#1");
        let opener = Self::editor_opener_for(caller);
        match self
            .kernel()
            .editor_open_board_signaled(
                &display_path,
                initial_text,
                crate::editor::BoardEditorTarget::NewTopic {
                    board_ref: board_ref.to_string(),
                    title: title.to_string(),
                    tags,
                },
                caller.clone(),
                opener,
            )
            .await
        {
            Ok((id, st)) => KjResult::ok_with_data(
                format!("opened editor session {} on {}", id.as_u64(), st.path),
                st.to_json(id),
            ),
            Err(e) => KjResult::Err(format!("kj board post --vi: {e}")),
        }
    }

    fn board_set_topic_status(
        &self,
        topic_ref: &str,
        new_status: TopicStatus,
        verb: &str,
        caller: &KjCaller,
    ) -> KjResult {
        let db = self.kernel_db().lock();
        let include_archived = new_status == TopicStatus::Open;
        let topic = match self.resolve_topic_with_archived(topic_ref, include_archived, caller, &db) {
            Ok(t) => t,
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };
        let board = match db.get_board(topic.board_id) {
            Ok(Some(b)) => b,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj board {verb}: board {} not found",
                    topic.board_id.short()
                ));
            }
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };

        let requires_mod = matches!(new_status, TopicStatus::Pinned | TopicStatus::Archived);
        if requires_mod {
            if let Err(e) = self.require_board_moderator(&board, caller, verb, &db) {
                return KjResult::Err(e);
            }
        } else {
            // `resolve` / `reopen`: topic creator or board moderator; locked board
            // still restricts writes to root/house.
            if board.write_policy == BoardWritePolicy::Locked {
                if !self.caller_is_root_or_house(caller, &db).unwrap_or(false) {
                    return KjResult::Err(format!(
                        "kj board {verb}: board is locked; only root or house can modify topics"
                    ));
                }
            } else if topic.created_by != caller.actor_id
                && let Err(e) = self.require_board_moderator(&board, caller, verb, &db)
            {
                return KjResult::Err(e);
            }
        }

        if let Err(e) = db.set_topic_status(topic.document_id, new_status) {
            return KjResult::Err(format!("kj board {verb}: {e}"));
        }

        KjResult::ok_with_data(
            format!(
                "topic '{}' ({}) is now {}",
                topic.slug,
                topic.document_id.short(),
                new_status
            ),
            serde_json::json!({
                "document_id": topic.document_id.to_hex(),
                "slug": topic.slug,
                "status": new_status.as_str(),
            }),
        )
    }

    fn board_unpin(&self, topic_ref: &str, caller: &KjCaller) -> KjResult {
        let db = self.kernel_db().lock();
        let topic = match self.resolve_topic(topic_ref, caller, &db) {
            Ok(t) => t,
            Err(e) => return KjResult::Err(format!("kj board unpin: {e}")),
        };
        let board = match db.get_board(topic.board_id) {
            Ok(Some(b)) => b,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj board unpin: board {} not found",
                    topic.board_id.short()
                ));
            }
            Err(e) => return KjResult::Err(format!("kj board unpin: {e}")),
        };
        if let Err(e) = self.require_board_moderator(&board, caller, "unpin", &db) {
            return KjResult::Err(e);
        }
        if topic.status != TopicStatus::Pinned {
            return KjResult::Err(format!(
                "kj board unpin: topic '{}' is {}, not pinned",
                topic.slug, topic.status
            ));
        }
        if let Err(e) = db.set_topic_status(topic.document_id, TopicStatus::Open) {
            return KjResult::Err(format!("kj board unpin: {e}"));
        }
        KjResult::ok_with_data(
            format!("unpinned topic '{}' ({})", topic.slug, topic.document_id.short()),
            serde_json::json!({
                "document_id": topic.document_id.to_hex(),
                "slug": topic.slug,
                "status": TopicStatus::Open.as_str(),
            }),
        )
    }

    fn board_set_topic_locked(&self, topic_ref: &str, locked: bool, caller: &KjCaller) -> KjResult {
        let verb = if locked { "lock" } else { "unlock" };
        let db = self.kernel_db().lock();
        let topic = match self.resolve_topic(topic_ref, caller, &db) {
            Ok(t) => t,
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };
        let board = match db.get_board(topic.board_id) {
            Ok(Some(b)) => b,
            Ok(None) => {
                return KjResult::Err(format!(
                    "kj board {verb}: board {} not found",
                    topic.board_id.short()
                ));
            }
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };
        if let Err(e) = self.require_board_moderator(&board, caller, verb, &db) {
            return KjResult::Err(e);
        }
        if let Err(e) = db.set_topic_locked(topic.document_id, locked) {
            return KjResult::Err(format!("kj board {verb}: {e}"));
        }
        let state = if locked { "locked" } else { "unlocked" };
        KjResult::ok_with_data(
            format!("{state} topic '{}' ({})", topic.slug, topic.document_id.short()),
            serde_json::json!({
                "document_id": topic.document_id.to_hex(),
                "slug": topic.slug,
                "locked": locked,
            }),
        )
    }

    fn board_set_post_hidden(&self, post_ref: &str, hidden: bool, caller: &KjCaller) -> KjResult {
        let verb = if hidden { "hide" } else { "unhide" };
        let (topic_part, post_num) = match parse_post_ref(post_ref) {
            Ok(pair) => pair,
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };

        let topic = {
            let db = self.kernel_db().lock();
            let topic = match self.resolve_topic(topic_part, caller, &db) {
                Ok(t) => t,
                Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
            };
            let board = match db.get_board(topic.board_id) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj board {verb}: board {} not found",
                        topic.board_id.short()
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
            };
            if let Err(e) = self.require_board_moderator(&board, caller, verb, &db) {
                return KjResult::Err(e);
            }
            topic
        };

        let snaps = match self.block_store().non_draft_snapshots(topic.document_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj board {verb}: {e}")),
        };
        let Some(target) = snaps.get(post_num - 1) else {
            return KjResult::Err(format!(
                "kj board {verb}: post #{post_num} does not exist in topic '{}' (has {} posts)",
                topic.slug,
                snaps.len()
            ));
        };

        if !hidden {
            let ask_state = {
                let db = self.kernel_db().lock();
                db.approval_for_board_post(&target.id)
                    .ok()
                    .flatten()
                    .map(|ask_id| {
                        let status = db.get_approval(&ask_id).ok().flatten().map(|r| r.status);
                        (ask_id, status)
                    })
            };
            if let Some((ask_id, status)) = ask_state {
                match status {
                    Some(s) if s.is_allowed() => {}
                    Some(s @ (crate::ApprovalStatus::Pending | crate::ApprovalStatus::Claimed)) => {
                        return KjResult::Err(format!(
                            "kj board unhide: post #{post_num} in topic '{}' is held pending review (ask '{ask_id}', status {s}); release it with `kj ledger allow '{ask_id}'`",
                            topic.slug
                        ));
                    }
                    Some(s) => {
                        return KjResult::Err(format!(
                            "kj board unhide: post #{post_num} in topic '{}' cannot be unhidden because review ask '{ask_id}' is {s}",
                            topic.slug
                        ));
                    }
                    None => {
                        return KjResult::Err(format!(
                            "kj board unhide: post #{post_num} in topic '{}' is bound to review ask '{ask_id}' whose approval record is missing",
                            topic.slug
                        ));
                    }
                }
            } else if target.status == Status::Pending && target.excluded {
                return KjResult::Err(format!(
                    "kj board unhide: post #{post_num} in topic '{}' is held pending review; release it through `kj ledger allow`",
                    topic.slug
                ));
            }
        }

        if let Err(e) = self.block_store().set_excluded(topic.document_id, &target.id, hidden) {
            return KjResult::Err(format!("kj board {verb}: {e}"));
        }

        let state = if hidden { "hid" } else { "restored" };
        KjResult::ok_with_data(
            format!("{state} post #{post_num} in topic '{}' ({})", topic.slug, topic.document_id.short()),
            serde_json::json!({
                "document_id": topic.document_id.to_hex(),
                "post_number": post_num,
                "block_id": target.id.to_key(),
                "excluded": hidden,
            }),
        )
    }

    // ========================================================================
    // Addressing resolution
    // ========================================================================

    fn resolve_board_caller_character(
        &self,
        caller: &KjCaller,
        verb: &str,
        db: &KernelDb,
    ) -> Result<CharacterRow, String> {
        match db.get_character(caller.actor_id) {
            Ok(Some(row)) => Ok(row),
            Ok(None) => Err(format!(
                "kj board {verb}: principal {} has no character; run `kj character create '<name>'` first",
                caller.actor_id.short()
            )),
            Err(e) => Err(format!("kj board {verb}: {e}")),
        }
    }

    fn resolve_board_read(
        &self,
        board_ref: &str,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<ReadBoard, String> {
        match board_ref {
            "." => {
                let ctx_id = caller.context_id.ok_or_else(|| {
                    "no active context joined; pass an explicit board ref like '<character>/<slug>'"
                        .to_string()
                })?;
                let ctx = db
                    .get_context(ctx_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("context {} has no row", ctx_id.short()))?;
                match ctx.board_id {
                    Some(bid) => db
                        .get_board(bid)
                        .map_err(|e| e.to_string())?
                        .map(ReadBoard::Found)
                        .ok_or_else(|| format!("context board {} not found", bid.short())),
                    None => Ok(ReadBoard::UncreatedContextBoard),
                }
            }
            ".senpai" => {
                let ctx_id = caller.context_id.ok_or_else(|| {
                    "no active context joined; pass an explicit board ref".to_string()
                })?;
                let senpai_id = db
                    .senpai_of(ctx_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "this context has no senpai context".to_string())?;
                let senpai = db
                    .get_context(senpai_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("senpai context {} has no row", senpai_id.short()))?;
                match senpai.board_id {
                    Some(bid) => db
                        .get_board(bid)
                        .map_err(|e| e.to_string())?
                        .map(ReadBoard::Found)
                        .ok_or_else(|| format!("senpai board {} not found", bid.short())),
                    None => Ok(ReadBoard::UncreatedContextBoard),
                }
            }
            other => self
                .resolve_named_board(other, caller, db)
                .map(ReadBoard::Found),
        }
    }

    fn resolve_board_write(
        &self,
        board_ref: &str,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<BoardRow, String> {
        match board_ref {
            "." => {
                let ctx_id = caller.context_id.ok_or_else(|| {
                    "no active context joined; pass an explicit board ref like '<character>/<slug>'"
                        .to_string()
                })?;
                db.ensure_context_board(ctx_id, caller.actor_id)
                    .map_err(|e| e.to_string())
            }
            ".senpai" => {
                let ctx_id = caller.context_id.ok_or_else(|| {
                    "no active context joined; pass an explicit board ref".to_string()
                })?;
                let senpai_id = db
                    .senpai_of(ctx_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "this context has no senpai context".to_string())?;
                let senpai = db
                    .get_context(senpai_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("senpai context {} has no row", senpai_id.short()))?;
                if let Some(bid) = senpai.board_id {
                    return db
                        .get_board(bid)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("senpai board {} not found", bid.short()));
                }
                if senpai.is_archived() {
                    return Err(format!(
                        "senpai context {} is archived and has no working board",
                        senpai_id.short()
                    ));
                }
                db.ensure_context_board(senpai_id, caller.actor_id)
                    .map_err(|e| e.to_string())
            }
            other => self.resolve_named_board(other, caller, db),
        }
    }

    fn resolve_named_board(
        &self,
        board_ref: &str,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<BoardRow, String> {
        let (owner, slug) = if let Some(slug) = board_ref.strip_prefix("self/") {
            let ch = db
                .get_character(caller.actor_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "principal {} has no character; pass '<character>/<slug>' explicitly",
                        caller.actor_id.short()
                    )
                })?;
            (ch, slug)
        } else if let Some((char_name, slug)) = board_ref.split_once('/') {
            if slug.contains('/') {
                return Err(format!(
                    "invalid board ref '{board_ref}': expected '<character>/<slug>'"
                ));
            }
            let ch = db
                .get_character_by_name(char_name)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "no character named '{char_name}'; run `kj character list` to see who exists"
                    )
                })?;
            (ch, slug)
        } else {
            let ch = db
                .get_character(caller.actor_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "principal {} has no character; pass '<character>/<slug>' explicitly",
                        caller.actor_id.short()
                    )
                })?;
            (ch, board_ref)
        };

        db.get_character_board_by_slug(owner.principal_id, slug)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "no board '{}/{slug}'; run `kj board create '{slug}'` first",
                    owner.name
                )
            })
    }

    fn resolve_topic(
        &self,
        topic_ref: &str,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<BoardTopicRow, String> {
        self.resolve_topic_with_archived(topic_ref, false, caller, db)
    }

    fn resolve_topic_with_archived(
        &self,
        topic_ref: &str,
        include_archived: bool,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<BoardTopicRow, String> {
        let lookup_slug = |board_id: BoardId, topic_slug: &str| {
            if include_archived {
                db.get_topic_by_slug_including_archived(board_id, topic_slug)
            } else {
                db.get_topic_by_slug(board_id, topic_slug)
            }
            .map_err(|e| e.to_string())
        };

        if let Some(topic_slug) = topic_ref.strip_prefix("./") {
            let board = match self.resolve_board_read(".", caller, db)? {
                ReadBoard::Found(b) => b,
                ReadBoard::UncreatedContextBoard => {
                    return Err(format!("no topic '{topic_ref}': current context has no board yet"));
                }
            };
            return lookup_slug(board.board_id, topic_slug)?
                .ok_or_else(|| format!("no topic '{topic_slug}' on current context board"));
        }

        if let Some(topic_slug) = topic_ref.strip_prefix(".senpai/") {
            let board = match self.resolve_board_read(".senpai", caller, db)? {
                ReadBoard::Found(b) => b,
                ReadBoard::UncreatedContextBoard => {
                    return Err(format!("no topic '{topic_ref}': senpai context has no board yet"));
                }
            };
            return lookup_slug(board.board_id, topic_slug)?
                .ok_or_else(|| format!("no topic '{topic_slug}' on senpai context board"));
        }

        let parts: Vec<&str> = topic_ref.split('/').collect();
        match parts.as_slice() {
            [hex_prefix] => db
                .resolve_topic_prefix(hex_prefix)
                .map_err(|e| e.to_string()),
            [board_slug, topic_slug] => {
                let board = self.resolve_named_board(board_slug, caller, db)?;
                lookup_slug(board.board_id, topic_slug)?
                    .ok_or_else(|| format!("no topic '{topic_slug}' on board '{board_slug}'"))
            }
            [char_or_self, board_slug, topic_slug] => {
                let board_ref = format!("{char_or_self}/{board_slug}");
                let board = self.resolve_named_board(&board_ref, caller, db)?;
                lookup_slug(board.board_id, topic_slug)?
                    .ok_or_else(|| format!("no topic '{topic_slug}' on board '{board_ref}'"))
            }
            _ => Err(format!(
                "invalid topic ref '{topic_ref}': expected './<slug>', '.senpai/<slug>', '<character>/<board>/<topic>', '<board>/<topic>', or a hex prefix"
            )),
        }
    }

    // ========================================================================
    // Authority & write-policy seam
    // ========================================================================

    /// True if caller is privileged, a live root character, or holds the
    /// `house` capability.
    fn caller_is_root_or_house(&self, caller: &KjCaller, db: &KernelDb) -> Result<bool, String> {
        if caller.privileged {
            return Ok(true);
        }
        if db
            .get_character(caller.actor_id)
            .map_err(|e| e.to_string())?
            .is_some_and(|c| c.is_live_root())
        {
            return Ok(true);
        }
        let Some(ctx) = caller.context_id else {
            return Ok(false);
        };
        db.get_context_binding(ctx)
            .map(|b| b.is_some_and(|b| b.allows(&crate::mcp::Capability::House)))
            .map_err(|e| e.to_string())
    }

    /// True if caller is root, `house`, or holds the `operator` capability.
    fn caller_is_root_house_or_operator(
        &self,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<bool, String> {
        if self.caller_is_root_or_house(caller, db)? {
            return Ok(true);
        }
        let Some(ctx) = caller.context_id else {
            return Ok(false);
        };
        db.get_context_binding(ctx)
            .map(|b| b.is_some_and(|b| b.allows(&crate::mcp::Capability::Operator)))
            .map_err(|e| e.to_string())
    }

    /// True if caller can moderate a character (is that character, on its
    /// senpai chain, or root/`house`/`operator`).
    fn can_moderate_character(
        &self,
        target_character: PrincipalId,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<bool, String> {
        if caller.actor_id == target_character {
            return Ok(true);
        }
        if self.caller_is_root_house_or_operator(caller, db)? {
            return Ok(true);
        }
        db.is_on_senpai_chain_of_character(caller.actor_id, caller.context_id, target_character)
            .map_err(|e| e.to_string())
    }

    /// True if caller is board owner, on owner's senpai chain, or
    /// root/`house`/`operator`.
    fn can_moderate_board(
        &self,
        board: &BoardRow,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<bool, String> {
        if let Some(owner_char) = board.owner_character {
            return self.can_moderate_character(owner_char, caller, db);
        }
        if self.caller_is_root_house_or_operator(caller, db)? {
            return Ok(true);
        }
        let owners = db
            .contexts_for_board(board.board_id)
            .map_err(|e| e.to_string())?;
        for ctx in &owners {
            if caller.context_id == Some(ctx.context_id)
                || ctx.performer == Some(caller.actor_id)
            {
                return Ok(true);
            }
            if db
                .is_on_senpai_chain_of_context(caller.actor_id, caller.context_id, ctx.context_id)
                .map_err(|e| e.to_string())?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn require_board_moderator(
        &self,
        board: &BoardRow,
        caller: &KjCaller,
        verb: &str,
        db: &KernelDb,
    ) -> Result<(), String> {
        if self.can_moderate_board(board, caller, db)? {
            return Ok(());
        }
        Err(format!(
            "kj board {verb}: requires board owner, senpai, house, or operator authority"
        ))
    }

    /// Whether a topic has at least one visible (non-excluded) post.
    fn topic_has_visible_posts(&self, document_id: ContextId) -> bool {
        if !self.block_store().contains(document_id) {
            let _ = self.block_store().load_one_from_db(document_id);
        }
        self.block_store()
            .non_draft_snapshots(document_id)
            .map(|snaps| snaps.iter().any(|b| !b.excluded))
            .unwrap_or(false)
    }

    /// Write-policy decision for `post`, `reply`, and `promote`.
    ///
    /// - `Locked`: refuses unless caller is root or `house`.
    /// - `Open`: publishes directly unless `gate.toml` explicitly denies/asks
    ///   for this performer, in which case it enters the held-ask path.
    /// - `Curated`: publishes directly for board owner, senpai chain,
    ///   root/`house`/`operator`, or an explicit `gate.toml` `Allow` for this
    ///   performer; holds other callers for ledger review.
    fn publish_or_hold_post(
        &self,
        board: &BoardRow,
        caller: &KjCaller,
        verb: &str,
        gate_config: &crate::kj::gate_policy::GateConfig,
        db: &KernelDb,
    ) -> Result<WriteDecision, String> {
        let performer = crate::kj::gate_policy::performer_for_actor(db, caller.actor_id)?;
        let candidates = [format!("kj board {verb}"), "kj board".to_string()];
        let gate_override = gate_config.performer_override(performer.as_ref(), &candidates);

        match board.write_policy {
            BoardWritePolicy::Locked => {
                if self.caller_is_root_or_house(caller, db)? {
                    Ok(WriteDecision::PublishDirect)
                } else {
                    Err(format!(
                        "kj board {verb}: board is locked; only root or house can write to it"
                    ))
                }
            }
            BoardWritePolicy::Open => match gate_override {
                Some(ConfigTierOverride::Deny | ConfigTierOverride::Ask) => {
                    let reviewer = self.resolve_board_reviewer(board, caller, db)?;
                    Ok(WriteDecision::HoldForReview { reviewer })
                }
                _ => Ok(WriteDecision::PublishDirect),
            },
            BoardWritePolicy::Curated => {
                if self.can_moderate_board(board, caller, db)?
                    || gate_override == Some(ConfigTierOverride::Allow)
                {
                    Ok(WriteDecision::PublishDirect)
                } else {
                    let reviewer = self.resolve_board_reviewer(board, caller, db)?;
                    Ok(WriteDecision::HoldForReview { reviewer })
                }
            }
        }
    }

    /// Resolve who reviews a held post on `board`: the board's live owning
    /// character (when distinct from the caller), otherwise its senpai chain,
    /// otherwise the caller's senpai chain, otherwise any live root character.
    fn resolve_board_reviewer(
        &self,
        board: &BoardRow,
        caller: &KjCaller,
        db: &KernelDb,
    ) -> Result<PrincipalId, String> {
        if let Some(owner_id) = board.owner_character {
            if owner_id != caller.actor_id
                && db
                    .get_character(owner_id)
                    .map_err(|e| e.to_string())?
                    .is_some_and(|c| c.retired_at.is_none())
            {
                return Ok(owner_id);
            }
            // Owner is retired or is the caller themselves: climb from any live
            // context performed by the owner.
            let contexts = db.list_active_contexts().map_err(|e| e.to_string())?;
            for ctx in contexts.iter().filter(|c| c.performer == Some(owner_id)) {
                if let Ok(route) = db.route_review(crate::kernel_db::ReviewQuery::ask(
                    ctx.context_id,
                    caller.actor_id,
                )) {
                    let reviewer = route.reviewer();
                    if reviewer != caller.actor_id {
                        return Ok(reviewer);
                    }
                }
            }
        } else {
            // Context board: first live owning context's performer or its senpai.
            let owners = db
                .contexts_for_board(board.board_id)
                .map_err(|e| e.to_string())?;
            for ctx in owners.iter().filter(|c| !c.is_archived()) {
                if let Some(perf) = ctx.performer
                    && perf != caller.actor_id
                    && db
                        .get_character(perf)
                        .map_err(|e| e.to_string())?
                        .is_some_and(|c| c.retired_at.is_none())
                {
                    return Ok(perf);
                }
                if let Ok(route) = db.route_review(crate::kernel_db::ReviewQuery::ask(
                    ctx.context_id,
                    caller.actor_id,
                )) {
                    let reviewer = route.reviewer();
                    if reviewer != caller.actor_id {
                        return Ok(reviewer);
                    }
                }
            }
        }

        if let Some(ctx_id) = caller.context_id
            && let Ok(route) =
                db.route_review(crate::kernel_db::ReviewQuery::ask(ctx_id, caller.actor_id))
        {
            let reviewer = route.reviewer();
            if reviewer != caller.actor_id {
                return Ok(reviewer);
            }
        }

        if let Some(root) = db
            .list_characters(false)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|c| c.is_live_root() && c.principal_id != caller.actor_id)
        {
            return Ok(root.principal_id);
        }

        Err(format!(
            "no live reviewer is available for board '{}'; ensure its owner or a live root character exists",
            format_board_label(db, board.board_id)
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn open_board_post_ask(
        &self,
        board: &BoardRow,
        board_label: &str,
        document_id: ContextId,
        slug: &str,
        topic_title: &str,
        block_id: BlockId,
        content: &str,
        verb: &str,
        caller: &KjCaller,
        reviewer: PrincipalId,
    ) -> Result<String, String> {
        use approval_ledger::types::{NewAsk, NewOption, Origin};
        let preview = backref_preview(content);
        let request_id = {
            let db = self.kernel_db().lock();
            let ask = NewAsk {
                context_id: document_id.as_bytes().to_vec(),
                actor_id: caller.actor_id.as_bytes().to_vec(),
                reviewer_id: reviewer.as_bytes().to_vec(),
                principal_id: caller.principal_id.as_bytes().to_vec(),
                origin: Origin::Board,
                instance: Some("builtin.kj".into()),
                tool: Some(format!("board.{verb}")),
                hook_id: None,
                description: format!(
                    "board {verb} on '{board_label}/{slug}' (\"{topic_title}\"): {preview}"
                ),
                statements: vec![],
                authorized_label: None,
                rc_run_id: None,
                expires_at: None,
                options: vec![
                    NewOption {
                        option_id: "allow_once".into(),
                        label: "Publish post".into(),
                        kind: "allow_once".into(),
                    },
                    NewOption {
                        option_id: "deny".into(),
                        label: "Dismiss".into(),
                        kind: "deny".into(),
                    },
                ],
                signals: vec![],
                cwd: None,
                exec_source: None,
                exec_stdin: None,
                continuation_epoch: None,
                env: vec![],
            };
            db.create_approval_ask_recorded(&ask, false, |conn, request| {
                crate::kernel_db::record_approval_board_post(
                    conn,
                    request,
                    document_id,
                    &block_id,
                    board.board_id,
                )
            })
            .map_err(|e| format!("could not record the ask: {e}"))?
        };
        crate::kj::gate::announce_ledger_change(self.kernel_db(), self.kernel().ledger_flows());
        Ok(request_id)
    }
}

enum WriteDecision {
    PublishDirect,
    HoldForReview { reviewer: PrincipalId },
}

// ============================================================================
// Pure helpers
// ============================================================================

fn resolve_body_text(positional: &[String], content: Option<&str>) -> String {
    if !positional.is_empty() {
        positional.join(" ")
    } else {
        content.unwrap_or_default().to_string()
    }
}

fn validate_board_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("board slug must not be empty".to_string());
    }
    if matches!(slug, "." | ".." | ".senpai" | "self") {
        return Err(format!("board slug '{slug}' is reserved"));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid board slug '{slug}': use ASCII letters, digits, '-', or '_'"
        ));
    }
    Ok(())
}

fn slugify_title(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "topic".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unique_topic_slug(db: &KernelDb, board_id: BoardId, title: &str) -> Result<String, String> {
    let base = slugify_title(title);
    if db
        .get_topic_by_slug(board_id, &base)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Ok(base);
    }
    for n in 2..=10_000 {
        let candidate = format!("{base}-{n}");
        if db
            .get_topic_by_slug(board_id, &candidate)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Ok(candidate);
        }
    }
    Err(format!("could not allocate a unique slug for '{title}'"))
}

fn parse_post_ref(post_ref: &str) -> Result<(&str, usize), String> {
    let Some((topic, num_str)) = post_ref.rsplit_once('#') else {
        return Err(format!(
            "invalid post ref '{post_ref}': expected '<topic>#<n>' (for example 'a1b2c3d4#2')"
        ));
    };
    if topic.is_empty() {
        return Err(format!(
            "invalid post ref '{post_ref}': missing topic before '#'"
        ));
    }
    let num: usize = num_str.parse().map_err(|_| {
        format!("invalid post number '{num_str}' in '{post_ref}': expected a positive integer")
    })?;
    if num == 0 {
        return Err(format!(
            "invalid post ref '{post_ref}': post numbers are 1-based (start at 1)"
        ));
    }
    Ok((topic, num))
}

fn parse_duration_ms(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    let Some(unit) = s.chars().next_back() else {
        return Err(format!(
            "invalid duration '{raw}': use a number followed by s, m, h, d, or w (for example '24h')"
        ));
    };
    let digits = &s[..s.len() - unit.len_utf8()];
    let value: i64 = digits.parse().map_err(|_| {
        format!(
            "invalid duration '{raw}': use a number followed by s, m, h, d, or w (for example '24h')"
        )
    })?;
    if value <= 0 {
        return Err(format!("invalid duration '{raw}': value must be positive"));
    }
    let unit_ms: i64 = match unit {
        's' => 1_000,
        'm' => 60 * 1_000,
        'h' => 3_600 * 1_000,
        'd' => 86_400 * 1_000,
        'w' => 7 * 86_400 * 1_000,
        _ => {
            return Err(format!(
                "invalid duration unit '{unit}' in '{raw}': expected s, m, h, d, or w"
            ));
        }
    };
    value
        .checked_mul(unit_ms)
        .ok_or_else(|| format!("duration '{raw}' is too large"))
}

fn backref_preview(content: &str) -> String {
    let collapsed: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let prefix: String = chars.by_ref().take(BACKREF_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn format_board_label(db: &KernelDb, board_id: BoardId) -> String {
    let Ok(Some(board)) = db.get_board(board_id) else {
        return board_id.short();
    };
    match (board.owner_character, board.slug) {
        (Some(owner), Some(slug)) => format!("{}/{slug}", db.name_for(owner)),
        _ => format!("ctx:{}", board.board_id.short()),
    }
}

fn format_topic_line(topic: &BoardTopicRow, board_prefix: Option<&str>) -> String {
    let age = super::format::format_age_compact(topic.last_post_at);
    let lock = if topic.locked { " [locked]" } else { "" };
    let tags = if topic.tags.is_empty() {
        String::new()
    } else {
        format!(" ({})", topic.tags.join(", "))
    };
    let name = match board_prefix {
        Some(prefix) => format!("{prefix}/{}", topic.slug),
        None => topic.slug.clone(),
    };
    format!(
        "{}  [{:<8}]  {:>3}p  {:>4}  {}{}  \"{}\"{}",
        topic.document_id.short(),
        topic.status.as_str(),
        topic.post_count,
        age,
        name,
        lock,
        topic.title,
        tags,
    )
}

#[cfg(test)]
mod tests;
