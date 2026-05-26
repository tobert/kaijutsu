//! `kj block` — inspect blocks in a context.
//!
//! First kj namespace migrated to clap_derive. Pattern: one `BlockArgs`
//! struct + `BlockCommand` enum at the top, dispatch_block parses argv via
//! `try_parse_from`, then matches the variant to the per-verb function. The
//! existing function bodies stayed mostly intact — only argv extraction
//! moved into the derive.
//!
//! Routes through the same `BlockStore::block_snapshots` surface that powers
//! `block_list` / `block_inspect` MCP tools, exposed as kj subcommands so
//! kaish scripts (rc lifecycle, the live-eval harness) can read block state
//! without going through MCP. `read` closes the partial-parity gap with
//! `block_read` (line numbers + range filtering).

use clap::{Parser, Subcommand};
use kaijutsu_types::{BlockKind, ContentType, Role, Status};
use serde::Serialize;

use super::refs::resolve_context_arg;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "block",
    about = "Inspect blocks in a context",
    disable_help_subcommand = true,
    no_binary_name = true
)]
struct BlockArgs {
    #[command(subcommand)]
    command: BlockCommand,
}

#[derive(Subcommand, Debug)]
enum BlockCommand {
    /// List blocks in a context with optional filters.
    #[command(alias = "ls")]
    List {
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        #[arg(long, short = 'c')]
        context: Option<String>,
        /// Filter by kind: text|thinking|tool_call|tool_result|drift|file|error|notification|resource|trace
        #[arg(long)]
        kind: Option<String>,
        /// Filter by role: user|model|system|tool|asset
        #[arg(long)]
        role: Option<String>,
        /// Filter by status: pending|running|done|error
        #[arg(long)]
        status: Option<String>,
        /// Emit a single JSON object instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Inspect a single block's metadata.
    Inspect {
        /// Block id: context_hex_principal_hex_seq (or legacy : form)
        block_id: String,
        /// Emit a single JSON object instead of a labelled table
        #[arg(long)]
        json: bool,
    },
    /// Count blocks matching filters.
    Count {
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        #[arg(long, short = 'c')]
        context: Option<String>,
        /// Filter by kind
        #[arg(long)]
        kind: Option<String>,
        /// Filter by role
        #[arg(long)]
        role: Option<String>,
    },
    /// Read a block's full content. Mirrors MCP `block_read` — line numbers
    /// by default; `--range start:end` for half-open slices (0-indexed).
    Read {
        /// Block id
        block_id: String,
        /// Suppress line numbers (default: show them)
        #[arg(long = "no-line-numbers")]
        no_line_numbers: bool,
        /// Line range "start:end" — 0-indexed, end exclusive. Omit to read all.
        #[arg(long)]
        range: Option<String>,
    },
    /// Append text to a block (streaming-friendly). Mirrors MCP `block_append`.
    Append {
        /// Block id to append to
        block_id: String,
        /// Text to append
        #[arg(long)]
        text: String,
    },
    /// Show creation + version info for a block. Mirrors MCP `block_history`.
    History {
        /// Block id
        block_id: String,
    },
    /// Unified line-by-line diff of block content against original text.
    /// Mirrors MCP `block_diff`. Without --original, prints current content.
    Diff {
        /// Block id
        block_id: String,
        /// Original text to diff against (omit for current-content view)
        #[arg(long)]
        original: Option<String>,
    },
    /// Create a new block in a context. Mirrors MCP `block_create`. Status
    /// defaults to Done and content_type to plain — matches the MCP shape.
    Create {
        /// Role: user|model|system|tool
        #[arg(long)]
        role: String,
        /// Kind: text|thinking|tool_call|tool_result|drift|file|error|notification|resource|trace
        #[arg(long)]
        kind: String,
        /// Initial text content (empty if omitted)
        #[arg(long)]
        content: Option<String>,
        /// Parent block id for DAG relationship (omit for root)
        #[arg(long)]
        parent: Option<String>,
        /// Block id to insert after (for ordering)
        #[arg(long)]
        after: Option<String>,
        /// Target context: . (default) | .parent | <label> | <hex prefix>
        #[arg(long, short = 'c')]
        context: Option<String>,
    },
}

impl KjDispatcher {
    pub(crate) fn dispatch_block(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // No argv → render help (clap reports DisplayHelp from `--help`; this
        // covers bare `kj block` for parity with the old hand-rolled path).
        if argv.is_empty() {
            return clap_help_for::<BlockArgs>();
        }
        let parsed = match BlockArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                // `--help` / `-h` requests come through as DisplayHelp errors;
                // route them to ok-ephemeral so kaish prints them and exits 0.
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj block: {e}"));
            }
        };
        match parsed.command {
            BlockCommand::List {
                context,
                kind,
                role,
                status,
                json,
            } => self.block_list(context.as_deref(), kind.as_deref(), role.as_deref(), status.as_deref(), json, caller),
            BlockCommand::Inspect { block_id, json } => self.block_inspect(&block_id, json),
            BlockCommand::Count {
                context,
                kind,
                role,
            } => self.block_count(context.as_deref(), kind.as_deref(), role.as_deref(), caller),
            BlockCommand::Read {
                block_id,
                no_line_numbers,
                range,
            } => self.block_read(&block_id, !no_line_numbers, range.as_deref()),
            BlockCommand::Append { block_id, text } => {
                self.block_append(&block_id, &text, caller)
            }
            BlockCommand::History { block_id } => self.block_history(&block_id),
            BlockCommand::Diff {
                block_id,
                original,
            } => self.block_diff(&block_id, original.as_deref()),
            BlockCommand::Create {
                role,
                kind,
                content,
                parent,
                after,
                context,
            } => self.block_create(
                context.as_deref(),
                &role,
                &kind,
                content.as_deref().unwrap_or(""),
                parent.as_deref(),
                after.as_deref(),
                caller,
            ),
        }
    }

    fn block_list(
        &self,
        ctx_ref: Option<&str>,
        kind_arg: Option<&str>,
        role_arg: Option<&str>,
        status_arg: Option<&str>,
        json: bool,
        caller: &KjCaller,
    ) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj block list: {e}")),
            }
        };

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block list: {e}")),
        };

        let kf = kind_arg.and_then(parse_kind);
        let rf = role_arg.and_then(Role::from_str);
        let sf = status_arg.and_then(Status::from_str);

        let filtered: Vec<_> = snapshots
            .iter()
            .filter(|b| {
                kf.is_none_or(|k| b.kind == k)
                    && rf.is_none_or(|r| b.role == r)
                    && sf.is_none_or(|s| b.status == s)
            })
            .collect();

        // For-loop iteration payload: JSON array of block id strings so
        // `for b in $(kj block list); do echo $b; done` walks ids directly.
        let id_array: serde_json::Value = serde_json::Value::Array(
            filtered
                .iter()
                .map(|b| serde_json::Value::String(b.id.to_key()))
                .collect(),
        );

        if json {
            let rows: Vec<BlockListRow> = filtered
                .iter()
                .map(|b| BlockListRow {
                    block_id: b.id.to_key(),
                    parent_id: b.parent_id.map(|id| id.to_key()),
                    role: b.role.as_str().to_string(),
                    kind: b.kind.as_str().to_string(),
                    status: b.status.as_str().to_string(),
                    content_length: b.content.len(),
                })
                .collect();
            let out = serde_json::json!({
                "context_id": ctx_id.to_hex(),
                "count": rows.len(),
                "total": snapshots.len(),
                "blocks": rows,
            });
            return KjResult::ok_with_data(out.to_string(), id_array);
        }

        if filtered.is_empty() {
            return KjResult::ok_with_data("(no blocks)".to_string(), id_array);
        }
        let mut out = String::new();
        for b in &filtered {
            out.push_str(&format!(
                "{}  {}/{}  [{}]  {}\n",
                short_key(&b.id.to_key()),
                b.role.as_str(),
                b.kind.as_str(),
                b.status.as_str(),
                first_line_trunc(&b.content, 60),
            ));
        }
        KjResult::ok_with_data(out, id_array)
    }

    fn block_inspect(&self, id_str: &str, json: bool) -> KjResult {
        // Round-trip with the keys `block list` emits: `BlockId::to_key()`
        // uses `_` (legacy `:` still accepted by from_key). Without this,
        // `for b in $(kj block list); do kj block inspect $b; done` would
        // reject every iteration as malformed.
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block inspect: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block inspect: {e}")),
        };
        let block_count = snapshots.len();
        let snap = match snapshots.iter().find(|b| b.id == block_id) {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block inspect: block '{id_str}' not found in {}",
                    ctx_id.to_hex()
                ));
            }
        };

        // Single-record inspect: the structured payload is the same JSON
        // object that `--json` prints, so `kaish-last` exposes the full
        // record after a plain `kj block inspect <id>`.
        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "context_block_count": block_count,
            "role": snap.role.as_str(),
            "kind": snap.kind.as_str(),
            "status": snap.status.as_str(),
            "parent_id": snap.parent_id.map(|id| id.to_key()),
            "content_length": snap.content.len(),
            "tool_name": snap.tool_name,
            "tool_call_id": snap.tool_call_id.map(|id| id.to_key()),
            "is_error": snap.is_error,
            "exit_code": snap.exit_code,
        });

        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }
        let parent = snap
            .parent_id
            .map(|i| i.to_key())
            .unwrap_or_else(|| "-".into());
        let out = format!(
            "id:        {}\nctx:       {}\nctx_count: {}\nrole:      {}\nkind:      {}\nstatus:    {}\nparent:    {}\ncontent:   {} chars\n",
            id_str,
            ctx_id.to_hex(),
            block_count,
            snap.role.as_str(),
            snap.kind.as_str(),
            snap.status.as_str(),
            parent,
            snap.content.len(),
        );
        KjResult::ok_with_data(out, record)
    }

    fn block_count(
        &self,
        ctx_ref: Option<&str>,
        kind_arg: Option<&str>,
        role_arg: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj block count: {e}")),
            }
        };

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block count: {e}")),
        };
        let kf = kind_arg.and_then(parse_kind);
        let rf = role_arg.and_then(Role::from_str);
        let n = snapshots
            .iter()
            .filter(|b| kf.is_none_or(|k| b.kind == k) && rf.is_none_or(|r| b.role == r))
            .count();
        KjResult::ok_with_data(n.to_string(), serde_json::json!(n))
    }

    /// Read a block's content. Closes the MCP `block_read` parity gap
    /// (line numbers + range filtering) — kj inspect only shows metadata,
    /// this returns the body.
    fn block_read(&self, id_str: &str, line_numbers: bool, range: Option<&str>) -> KjResult {
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block read: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block read: {e}")),
        };
        let snap = match snapshots.iter().find(|b| b.id == block_id) {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block read: block '{id_str}' not found in {}",
                    ctx_id.to_hex()
                ));
            }
        };

        // Range parse: "start:end" — 0-indexed, end exclusive (mirrors
        // BlockReadRequest.range in kaijutsu-mcp's models.rs).
        let (start, end) = match range {
            None => (0usize, usize::MAX),
            Some(spec) => match parse_range_spec(spec) {
                Ok(pair) => pair,
                Err(e) => return KjResult::Err(format!("kj block read: {e}")),
            },
        };

        let all_lines: Vec<&str> = snap.content.split('\n').collect();
        let total = all_lines.len();
        let end_clamped = end.min(total);
        if start > end_clamped {
            return KjResult::Err(format!(
                "kj block read: range start {start} > clamped end {end_clamped} (block has {total} lines)"
            ));
        }
        let slice = &all_lines[start..end_clamped];

        let mut out = String::new();
        for (i, line) in slice.iter().enumerate() {
            if line_numbers {
                // Display 1-indexed line numbers (matches MCP block_read
                // convention; range itself stays 0-indexed for slicing).
                let lineno = start + i + 1;
                out.push_str(&format!("{:>5}  {}\n", lineno, line));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }

        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "kind": snap.kind.as_str(),
            "role": snap.role.as_str(),
            "total_lines": total,
            "range_start": start,
            "range_end": end_clamped,
            "content_length": snap.content.len(),
        });
        KjResult::ok_with_data(out, record)
    }

    /// Append text to an existing block. Mirrors MCP `block_append`. Returns
    /// the new content length so callers can confirm the write took.
    fn block_append(&self, id_str: &str, text: &str, caller: &KjCaller) -> KjResult {
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block append: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;

        // append_text_as takes Option<PrincipalId>; pass the caller's so
        // the op is attributed to whoever invoked kj, not the system agent.
        if let Err(e) =
            self.blocks
                .append_text_as(ctx_id, &block_id, text, Some(caller.principal_id))
        {
            return KjResult::Err(format!("kj block append: {e}"));
        }

        // Read back to compute the new content length for the structured
        // record. Cheaper than tracking it via the append op signature.
        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block append: {e}")),
        };
        let new_len = snapshots
            .iter()
            .find(|b| b.id == block_id)
            .map(|s| s.content.len())
            .unwrap_or(0);

        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "appended_bytes": text.len(),
            "content_length": new_len,
        });
        KjResult::ok_with_data(format!("appended {} bytes\n", text.len()), record)
    }

    /// Version / creation info for a block. Mirrors MCP `block_history`.
    fn block_history(&self, id_str: &str) -> KjResult {
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block history: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block history: {e}")),
        };
        let snap = match snapshots.iter().find(|b| b.id == block_id) {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block history: block '{id_str}' not found in {}",
                    ctx_id.to_hex()
                ));
            }
        };
        // `version` here is the document-level CRDT version, matching the
        // MCP block_history semantics. Single-block oplog isn't surfaced
        // by the BlockStore today; if we add it, swap this for the
        // block-specific version.
        let version = self.blocks.version(ctx_id).unwrap_or(0);
        let content_lines = snap.content.lines().count().max(1);

        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "created_at_ms": snap.created_at,
            "author": snap.author().to_hex(),
            "document_version": version,
            "content_lines": content_lines,
            "content_bytes": snap.content.len(),
            "status": snap.status.as_str(),
        });
        let out = format!(
            "block:   {id}\n\
             created: {created}ms (unix epoch) by {author}\n\
             version: {version} (document)\n\
             content: {lines} line{lp}, {bytes} byte{bp}\n\
             status:  {status}\n",
            id = id_str,
            created = snap.created_at,
            author = snap.author().to_hex(),
            version = version,
            lines = content_lines,
            lp = if content_lines == 1 { "" } else { "s" },
            bytes = snap.content.len(),
            bp = if snap.content.len() == 1 { "" } else { "s" },
            status = snap.status.as_str(),
        );
        KjResult::ok_with_data(out, record)
    }

    /// Unified line-by-line diff against an original. Mirrors MCP
    /// `block_diff`. Without --original, prints current content.
    fn block_diff(&self, id_str: &str, original: Option<&str>) -> KjResult {
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block diff: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block diff: {e}")),
        };
        let snap = match snapshots.iter().find(|b| b.id == block_id) {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block diff: block '{id_str}' not found in {}",
                    ctx_id.to_hex()
                ));
            }
        };
        let current = &snap.content;

        let original = match original {
            None => {
                // No original — preview current content. Useful by itself.
                let out = format!(
                    "block: {id}\n\
                     {sep}\n\
                     no original text — showing current ({lines} lines, {bytes} bytes):\n\n\
                     {content}\n",
                    id = id_str,
                    sep = "─".repeat(40),
                    lines = current.lines().count(),
                    bytes = current.len(),
                    content = current,
                );
                let record = serde_json::json!({
                    "block_id": id_str,
                    "context_id": ctx_id.to_hex(),
                    "has_original": false,
                    "current_lines": current.lines().count(),
                    "current_bytes": current.len(),
                });
                return KjResult::ok_with_data(out, record);
            }
            Some(s) => s,
        };

        let orig_lines: Vec<&str> = original.lines().collect();
        let curr_lines: Vec<&str> = current.lines().collect();
        let max_lines = orig_lines.len().max(curr_lines.len());

        let mut out = format!("diff {id_str}\n{}\n", "─".repeat(40));
        let mut added = 0usize;
        let mut removed = 0usize;
        let mut changed = 0usize;
        for i in 0..max_lines {
            match (orig_lines.get(i).copied(), curr_lines.get(i).copied()) {
                (Some(o), Some(c)) if o == c => {
                    out.push_str(&format!("  {o}\n"));
                }
                (Some(o), Some(c)) => {
                    out.push_str(&format!("- {o}\n"));
                    out.push_str(&format!("+ {c}\n"));
                    changed += 1;
                }
                (Some(o), None) => {
                    out.push_str(&format!("- {o}\n"));
                    removed += 1;
                }
                (None, Some(c)) => {
                    out.push_str(&format!("+ {c}\n"));
                    added += 1;
                }
                (None, None) => {}
            }
        }
        if added + removed + changed == 0 {
            out.push_str("(no changes)\n");
        }

        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "has_original": true,
            "added_lines": added,
            "removed_lines": removed,
            "changed_lines": changed,
        });
        KjResult::ok_with_data(out, record)
    }

    /// Create a new block. Mirrors `block_create` MCP tool semantics: status
    /// defaults to Done, content_type to Plain. Returns the new block's id
    /// in both the rendered text and the structured `data` payload so the
    /// id is iterable (`for id in $(kj block create ...)`).
    fn block_create(
        &self,
        ctx_ref: Option<&str>,
        role: &str,
        kind: &str,
        content: &str,
        parent: Option<&str>,
        after: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj block create: {e}")),
            }
        };
        let role_p = match Role::from_str(role) {
            Some(r) => r,
            None => {
                return KjResult::Err(format!(
                    "kj block create: invalid role '{role}' (expected user|model|system|tool|asset)"
                ));
            }
        };
        let kind_p = match parse_kind(kind) {
            Some(k) => k,
            None => {
                return KjResult::Err(format!(
                    "kj block create: invalid kind '{kind}' (expected text|thinking|tool_call|tool_result|drift|file|error|notification|resource|trace)"
                ));
            }
        };
        let parent_id = match parent {
            None => None,
            Some(s) => match kaijutsu_types::BlockId::from_key(s) {
                Some(id) => Some(id),
                None => {
                    return KjResult::Err(format!(
                        "kj block create: malformed --parent id '{s}'"
                    ));
                }
            },
        };
        let after_id = match after {
            None => None,
            Some(s) => match kaijutsu_types::BlockId::from_key(s) {
                Some(id) => Some(id),
                None => {
                    return KjResult::Err(format!("kj block create: malformed --after id '{s}'"));
                }
            },
        };

        let new_id = match self.blocks.insert_block_as(
            ctx_id,
            parent_id.as_ref(),
            after_id.as_ref(),
            role_p,
            kind_p,
            content,
            Status::Done,
            ContentType::Plain,
            Some(caller.principal_id),
        ) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj block create: {e}")),
        };

        // Iteration payload is an array of the single new id, matching the
        // `list` shape so `for id in $(kj block create ...); do …; done`
        // walks the one block without needing jq.
        let key = new_id.to_key();
        KjResult::ok_with_data(
            format!("{key}\n"),
            serde_json::Value::Array(vec![serde_json::Value::String(key)]),
        )
    }
}

/// Render the auto-generated clap help text for a parser without going
/// through `try_parse_from`. Used when argv is empty so we can return the
/// command's full help instead of clap's parse-error for missing subcommand.
fn clap_help_for<T: clap::CommandFactory>() -> KjResult {
    let mut cmd = T::command();
    KjResult::ok_ephemeral(cmd.render_help().to_string(), ContentType::Plain)
}

/// Parse "start:end" into (start, end), end exclusive. Either side may be
/// empty: ":10" → (0, 10), "5:" → (5, usize::MAX). Errors on missing colon,
/// non-numeric parts, or end < start.
fn parse_range_spec(spec: &str) -> Result<(usize, usize), String> {
    let (lhs, rhs) = spec
        .split_once(':')
        .ok_or_else(|| format!("range '{spec}' must contain ':' (e.g. '5:10')"))?;
    let start: usize = if lhs.is_empty() {
        0
    } else {
        lhs.parse()
            .map_err(|_| format!("range start '{lhs}' is not a non-negative integer"))?
    };
    let end: usize = if rhs.is_empty() {
        usize::MAX
    } else {
        rhs.parse()
            .map_err(|_| format!("range end '{rhs}' is not a non-negative integer"))?
    };
    if end < start {
        return Err(format!("range end {end} < start {start}"));
    }
    Ok((start, end))
}

#[derive(Serialize)]
struct BlockListRow {
    block_id: String,
    parent_id: Option<String>,
    role: String,
    kind: String,
    status: String,
    content_length: usize,
}

fn parse_kind(s: &str) -> Option<BlockKind> {
    match s.to_ascii_lowercase().as_str() {
        "text" => Some(BlockKind::Text),
        "thinking" => Some(BlockKind::Thinking),
        "tool_call" | "toolcall" => Some(BlockKind::ToolCall),
        "tool_result" | "toolresult" => Some(BlockKind::ToolResult),
        "drift" => Some(BlockKind::Drift),
        "file" => Some(BlockKind::File),
        "error" => Some(BlockKind::Error),
        "notification" => Some(BlockKind::Notification),
        "resource" => Some(BlockKind::Resource),
        "trace" => Some(BlockKind::Trace),
        _ => None,
    }
}

fn short_key(s: &str) -> String {
    if s.len() > 16 {
        format!("{}…", &s[..16])
    } else {
        s.to_string()
    }
}

fn first_line_trunc(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or("").to_string();
    if one_line.chars().count() <= max {
        one_line
    } else {
        let trunc: String = one_line.chars().take(max).collect();
        format!("{trunc}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{BlockKind, ContentType, DocKind, PrincipalId, Role as TypesRole, Status};

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// `test_helpers::register_context` only registers in KernelDb. Production
    /// `create_context` (server/src/rpc.rs) also calls
    /// `BlockStore::create_document` — block ops need that. Wrap both.
    fn register_context_with_doc(
        d: &crate::kj::KjDispatcher,
        label: Option<&str>,
        principal: PrincipalId,
    ) -> kaijutsu_types::ContextId {
        let ctx = register_context(d, label, None, principal);
        d.block_store()
            .create_document(ctx, DocKind::Conversation, None)
            .expect("create_document");
        ctx
    }

    /// Insert a Text block directly via BlockStore so block_read tests have
    /// real content to slice. Returns the inserted block id.
    fn insert_text_block(
        d: &crate::kj::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        content: &str,
    ) -> kaijutsu_types::BlockId {
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                TypesRole::User,
                BlockKind::Text,
                content,
                Status::Done,
                ContentType::Plain,
                None,
            )
            .expect("insert_block_as")
    }

    // ── Existing behavior preserved ───────────────────────────────────

    #[tokio::test]
    async fn block_list_empty_context_json() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("list"), s("--json")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());

        let v: serde_json::Value =
            serde_json::from_str(result.message()).expect("output must be JSON");
        assert_eq!(v["count"], 0);
        assert_eq!(v["total"], 0);
        assert!(v["blocks"].is_array());
        assert_eq!(v["blocks"].as_array().unwrap().len(), 0);
        assert_eq!(v["context_id"], ctx.to_hex());
    }

    #[tokio::test]
    async fn block_count_empty_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("count")], &c).await;
        assert!(result.is_ok(), "count failed: {}", result.message());
        assert_eq!(result.message().trim(), "0");
    }

    #[tokio::test]
    async fn block_list_unknown_subcommand_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("nonsense")], &c).await;
        assert!(!result.is_ok());
        // clap reports `unrecognized subcommand` (or similar) for the unknown
        // verb; the legacy hand-rolled path said `unknown subcommand`. Both
        // messages mention the bad input — that's the testable contract.
        assert!(
            result.message().contains("nonsense"),
            "expected error to mention 'nonsense', got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_inspect_missing_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("inspect")], &c).await;
        assert!(!result.is_ok());
        // clap's missing-required-arg message names the missing argument.
        assert!(
            result.message().to_lowercase().contains("block_id")
                || result.message().to_lowercase().contains("required"),
            "expected error about missing required block_id, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_inspect_malformed_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("block"), s("inspect"), s("not-a-real-id")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("malformed"));
    }

    /// `kj block list` must populate `KjResult::Ok::data` with a JSON array
    /// of block-id strings so kaish's command-substitution path can iterate
    /// in `for b in $(kj block list)`. The text output is independent.
    #[tokio::test]
    async fn block_list_emits_structured_data_for_iteration() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("list")], &c).await;
        match result {
            KjResult::Ok { data, .. } => {
                let arr = data
                    .as_ref()
                    .and_then(|v| v.as_array())
                    .expect("empty block list must still emit an array (length 0)");
                assert_eq!(arr.len(), 0, "no blocks were inserted: {arr:?}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_count_emits_numeric_data() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("count")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v.as_i64(), Some(0), "expected zero count, got {v}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// `ls` alias must dispatch to `list`. clap `#[command(alias = "ls")]`
    /// gives us this; the test guards against an accidental removal.
    #[tokio::test]
    async fn block_list_ls_alias_works() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("ls"), s("--json")], &c).await;
        assert!(result.is_ok(), "ls alias failed: {}", result.message());
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["count"], 0);
    }

    // ── New: block read ────────────────────────────────────────────────

    #[tokio::test]
    async fn block_read_returns_full_content_with_line_numbers() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, "alpha\nbeta\ngamma");
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("block"), s("read"), bid.to_key()], &c)
            .await;
        assert!(result.is_ok(), "read failed: {}", result.message());
        let body = result.message();
        // Line numbers are 1-indexed for display.
        assert!(body.contains("    1  alpha"), "missing line 1: {body}");
        assert!(body.contains("    2  beta"), "missing line 2: {body}");
        assert!(body.contains("    3  gamma"), "missing line 3: {body}");
    }

    #[tokio::test]
    async fn block_read_no_line_numbers_strips_prefix() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, "alpha\nbeta");
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("block"), s("read"), bid.to_key(), s("--no-line-numbers")],
                &c,
            )
            .await;
        assert!(result.is_ok());
        let body = result.message();
        assert_eq!(body, "alpha\nbeta\n", "raw lines: {body:?}");
    }

    #[tokio::test]
    async fn block_read_range_slices_lines() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, "0\n1\n2\n3\n4");
        let c = caller_with_context(ctx);

        // Range "1:3" → lines at indices 1 and 2 ("1" and "2"). End exclusive.
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("read"),
                    bid.to_key(),
                    s("--range"),
                    s("1:3"),
                    s("--no-line-numbers"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "range read failed: {}", result.message());
        assert_eq!(result.message(), "1\n2\n", "got: {:?}", result.message());
    }

    #[tokio::test]
    async fn block_read_range_open_ended_works() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, "a\nb\nc");
        let c = caller_with_context(ctx);

        // ":2" → first two lines (0..2 exclusive). Matches MCP block_read.
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("read"),
                    bid.to_key(),
                    s("--range"),
                    s(":2"),
                    s("--no-line-numbers"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.message(), "a\nb\n");
    }

    #[tokio::test]
    async fn block_read_emits_structured_metadata_record() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, "x\ny\nz");
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("block"), s("read"), bid.to_key()], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["total_lines"], 3);
                assert_eq!(v["range_start"], 0);
                assert_eq!(v["range_end"], 3);
                assert_eq!(v["kind"], "text");
                assert_eq!(v["role"], "user");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_read_missing_block_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        // Construct a syntactically-valid id that points at no block.
        let phantom = kaijutsu_types::BlockId {
            context_id: ctx,
            agent_id: PrincipalId::new(),
            seq: 999,
        };
        let result = d
            .dispatch(&[s("block"), s("read"), phantom.to_key()], &c)
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("not found"),
            "expected 'not found', got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_read_malformed_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("block"), s("read"), s("garbage")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("malformed"));
    }

    // ── New: block create ─────────────────────────────────────────────

    #[tokio::test]
    async fn block_create_inserts_text_block_and_returns_id() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("create"),
                    s("--role"),
                    s("user"),
                    s("--kind"),
                    s("text"),
                    s("--content"),
                    s("hello from kj"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        // The text output is the new block id with a trailing newline.
        let key = result.message().trim();
        let parsed = kaijutsu_types::BlockId::from_key(key)
            .expect("emitted id must be a valid BlockId key");
        assert_eq!(parsed.context_id, ctx, "block id context mismatch");

        // The block is actually in the store with the right content + role.
        let snapshots = d.block_store().block_snapshots(ctx).unwrap();
        let snap = snapshots
            .iter()
            .find(|b| b.id == parsed)
            .expect("created block must be in store");
        assert_eq!(snap.content, "hello from kj");
        assert_eq!(snap.role, TypesRole::User);
        assert_eq!(snap.kind, BlockKind::Text);
        assert_eq!(snap.status, Status::Done);
    }

    #[tokio::test]
    async fn block_create_empty_content_defaults_ok() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;

        let result = d
            .dispatch(
                &[s("block"), s("create"), s("--role"), s("user"), s("--kind"), s("text")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "empty-content create failed: {}", result.message());
        let key = result.message().trim();
        let id = kaijutsu_types::BlockId::from_key(key).unwrap();
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == id)
            .unwrap();
        assert!(snap.content.is_empty(), "expected empty content");
    }

    #[tokio::test]
    async fn block_create_data_is_iterable_array() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;

        let result = d
            .dispatch(
                &[s("block"), s("create"), s("--role"), s("user"), s("--kind"), s("text"), s("--content"), s("x")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("data must be an array");
                assert_eq!(arr.len(), 1, "single new id, got: {arr:?}");
                assert!(arr[0].is_string());
            }
            other => panic!("expected Ok with array data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_create_invalid_role_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("block"), s("create"), s("--role"), s("bogus"), s("--kind"), s("text")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("invalid role"),
            "expected 'invalid role' error, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_create_invalid_kind_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("block"), s("create"), s("--role"), s("user"), s("--kind"), s("notakind")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("invalid kind"),
            "expected 'invalid kind' error, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_create_with_parent_links_dag() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;

        // First block as the parent.
        let parent_id = insert_text_block(&d, ctx, "root");
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("create"),
                    s("--role"),
                    s("user"),
                    s("--kind"),
                    s("text"),
                    s("--content"),
                    s("child"),
                    s("--parent"),
                    parent_id.to_key(),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create-with-parent failed: {}", result.message());

        let child_key = result.message().trim();
        let child_id = kaijutsu_types::BlockId::from_key(child_key).unwrap();
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == child_id)
            .unwrap();
        assert_eq!(snap.parent_id, Some(parent_id), "parent edge not set");
    }

    // ── New: block append ──────────────────────────────────────────────

    #[tokio::test]
    async fn block_append_extends_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "hello");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("append"),
                    bid.to_key(),
                    s("--text"),
                    s(" world"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "append failed: {}", result.message());

        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "hello world", "content not appended");
    }

    #[tokio::test]
    async fn block_append_emits_size_record() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "abc");

        let result = d
            .dispatch(
                &[s("block"), s("append"), bid.to_key(), s("--text"), s("def")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["appended_bytes"], 3);
                assert_eq!(v["content_length"], 6);
                assert_eq!(v["block_id"], bid.to_key());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_append_malformed_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("block"), s("append"), s("garbage"), s("--text"), s("x")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("malformed"));
    }

    // ── New: block history ────────────────────────────────────────────

    #[tokio::test]
    async fn block_history_returns_metadata() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "first\nsecond");

        let result = d
            .dispatch(&[s("block"), s("history"), bid.to_key()], &c)
            .await;
        assert!(result.is_ok(), "history failed: {}", result.message());

        let body = result.message();
        assert!(body.contains("block:"), "missing 'block:' header: {body}");
        assert!(body.contains("created:"), "missing 'created:': {body}");
        assert!(body.contains("version:"), "missing 'version:': {body}");
        assert!(body.contains("2 lines"), "wrong line count: {body}");

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["block_id"], bid.to_key());
                assert_eq!(v["content_lines"], 2);
                assert_eq!(v["content_bytes"], 12);
                assert!(v["document_version"].is_number());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_history_malformed_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("block"), s("history"), s("notanid")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("malformed"));
    }

    // ── New: block diff ───────────────────────────────────────────────

    #[tokio::test]
    async fn block_diff_no_original_shows_current() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "alpha\nbeta");

        let result = d
            .dispatch(&[s("block"), s("diff"), bid.to_key()], &c)
            .await;
        assert!(result.is_ok());
        let body = result.message();
        assert!(body.contains("no original"), "missing fallback note: {body}");
        assert!(body.contains("alpha"));
        assert!(body.contains("beta"));
    }

    #[tokio::test]
    async fn block_diff_against_original_renders_unified_diff() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "alpha\nbeta\nDELTA");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("diff"),
                    bid.to_key(),
                    s("--original"),
                    s("alpha\nbeta\ngamma"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok());
        let body = result.message();
        // unchanged line prefixed with two spaces; changed line shows -/+ pair
        assert!(body.contains("  alpha"), "alpha unchanged: {body}");
        assert!(body.contains("  beta"), "beta unchanged: {body}");
        assert!(body.contains("- gamma"), "gamma removed: {body}");
        assert!(body.contains("+ DELTA"), "DELTA added: {body}");

        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["has_original"], true);
                assert_eq!(v["changed_lines"], 1);
                assert_eq!(v["added_lines"], 0);
                assert_eq!(v["removed_lines"], 0);
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_diff_identical_reports_no_changes() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "same\nsame");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("diff"),
                    bid.to_key(),
                    s("--original"),
                    s("same\nsame"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("(no changes)"),
            "expected '(no changes)' marker: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_diff_pure_addition() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "a\nb\nc");

        // Original has fewer lines → added lines counted as additions.
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("diff"),
                    bid.to_key(),
                    s("--original"),
                    s("a"),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["added_lines"], 2, "b and c were added");
                assert_eq!(v["removed_lines"], 0);
                assert_eq!(v["changed_lines"], 0);
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    // ── Range spec parser unit tests ───────────────────────────────────

    #[test]
    fn parse_range_spec_basic() {
        assert_eq!(parse_range_spec("0:5").unwrap(), (0, 5));
        assert_eq!(parse_range_spec("10:20").unwrap(), (10, 20));
    }

    #[test]
    fn parse_range_spec_open_ended() {
        assert_eq!(parse_range_spec(":7").unwrap(), (0, 7));
        let (start, end) = parse_range_spec("3:").unwrap();
        assert_eq!(start, 3);
        assert_eq!(end, usize::MAX);
    }

    #[test]
    fn parse_range_spec_rejects_missing_colon() {
        let err = parse_range_spec("5").unwrap_err();
        assert!(err.contains(":"), "{err}");
    }

    #[test]
    fn parse_range_spec_rejects_inverted_range() {
        let err = parse_range_spec("10:5").unwrap_err();
        assert!(err.contains("end"));
    }

    #[test]
    fn parse_range_spec_rejects_non_numeric() {
        assert!(parse_range_spec("a:5").is_err());
        assert!(parse_range_spec("0:b").is_err());
    }
}
