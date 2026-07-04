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
use kaijutsu_cas::ContentStore;
use kaijutsu_types::{BlockKind, ContentType, Role, Status};
use serde::Serialize;

use crate::block_tools::translate::{line_range_to_byte_range, line_to_byte_offset};
use super::refs::resolve_context_arg;
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "block",
    about = "Inspect blocks in a context",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct BlockArgs {
    #[command(subcommand)]
    command: BlockCommand,
}

#[derive(Subcommand, Debug)]
enum EditOp {
    /// Insert text before line N (0-indexed). N == line_count appends.
    Insert {
        /// Line to insert before (0-indexed; equals line_count to append)
        #[arg(long)]
        line: u32,
        /// Text to insert. A trailing newline is added if missing.
        #[arg(long)]
        content: String,
    },
    /// Delete lines [start, end) — end exclusive, 0-indexed.
    Delete {
        /// First line to delete (0-indexed, inclusive)
        #[arg(long = "start")]
        start_line: u32,
        /// First line past the deletion (0-indexed, exclusive)
        #[arg(long = "end")]
        end_line: u32,
    },
    /// Replace lines [start, end) with new content. `--expected` adds
    /// compare-and-set validation against the current text in that range.
    Replace {
        /// First line to replace (0-indexed, inclusive)
        #[arg(long = "start")]
        start_line: u32,
        /// First line past the replacement (0-indexed, exclusive)
        #[arg(long = "end")]
        end_line: u32,
        /// Replacement content. A trailing newline is added if missing.
        #[arg(long)]
        content: String,
        /// CAS — fail unless the current range matches this text exactly
        #[arg(long)]
        expected: Option<String>,
    },
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
    /// One-step blob readback: resolve a block's payload and print or save
    /// it, following the CAS reference when the block is a derived/asset
    /// sibling (e.g. an ABC→MIDI render) instead of a hand-assembled
    /// `inspect` → `cas get` chain. Textual payloads print to stdout;
    /// binary payloads require `--out` (refused otherwise — a terminal
    /// should never eat raw binary).
    Cat {
        /// Block id to read (mutually exclusive with `--latest`)
        block_id: Option<String>,
        /// Select the newest block whose resolved mime matches this value
        /// (timeline order), instead of naming a specific id — e.g.
        /// `--latest audio/midi` for "the rendered artifact for this turn".
        #[arg(long, conflicts_with = "block_id")]
        latest: Option<String>,
        /// Context to search with `--latest` (defaults to the active context)
        #[arg(long, short = 'c')]
        context: Option<String>,
        /// Write the payload to this path instead of stdout
        #[arg(long)]
        out: Option<String>,
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
    /// Set the status field on a block. Mirrors MCP `block_status`.
    Status {
        /// Block id
        block_id: String,
        /// New status: pending|running|done|error
        new_status: String,
    },
    /// Edit a block via line-based operations. Single op per invocation —
    /// the kj surface trades MCP's batch-of-ops for a clean CLI shape.
    /// Mirrors MCP `block_edit` minus the multi-op atomicity (which the
    /// caller can recover with `kaish` script + `kj block read` between
    /// edits to confirm intermediate state).
    Edit {
        /// Block id
        block_id: String,
        #[command(subcommand)]
        op: EditOp,
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
        // Block mutations are gated on the matching `builtin.block` tool cap —
        // the kj surface checks the same capability the MCP tool would, so a
        // read-only loadout can't write blocks by routing through kj. Reads
        // (list/inspect/count/read/history/diff) stay ungated.
        let block_write_tool = match &parsed.command {
            BlockCommand::Append { .. } => Some("block_append"),
            BlockCommand::Edit { .. } => Some("block_edit"),
            BlockCommand::Create { .. } => Some("block_create"),
            BlockCommand::Status { .. } => Some("block_status"),
            _ => None,
        };
        if let Some(tool) = block_write_tool {
            let cap = crate::mcp::Capability::Tool {
                instance: crate::mcp::InstanceId::new("builtin.block"),
                tool: tool.to_string(),
            };
            if let Err(denied) = self.require_cap(caller, cap, "block") {
                return denied;
            }
        }
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
            BlockCommand::Cat {
                block_id,
                latest,
                context,
                out,
            } => self.block_cat(
                block_id.as_deref(),
                latest.as_deref(),
                context.as_deref(),
                out.as_deref(),
                caller,
            ),
            BlockCommand::Append { block_id, text } => {
                self.block_append(&block_id, &text, caller)
            }
            BlockCommand::Edit { block_id, op } => self.block_edit(&block_id, op, caller),
            BlockCommand::Status {
                block_id,
                new_status,
            } => self.block_status(&block_id, &new_status),
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
                short_key(&b.id),
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

    /// The CAS hash a block's content references, if it's a CAS-ref block by
    /// the `img_block`/materialize convention (`kaijutsu-hyoushigi::materialize`,
    /// `beat.rs::materialize_committed`): `Role::Asset` + a literal content
    /// string that parses as a 32-hex [`kaijutsu_cas::ContentHash`]. Any other
    /// block's `content` IS the payload, never a pointer — this returns `None`
    /// for it even if the text happens to look hex-ish, because `Role::Asset`
    /// is the load-bearing signal, not the string shape alone.
    fn block_cas_hash(
        &self,
        snap: &kaijutsu_types::BlockSnapshot,
    ) -> Option<kaijutsu_cas::ContentHash> {
        if snap.role != Role::Asset {
            return None;
        }
        snap.content.parse::<kaijutsu_cas::ContentHash>().ok()
    }

    /// The block's "real" mime for `--latest` matching: for a CAS-ref block,
    /// the mime recorded in the CAS sidecar (the actual bytes' type — a
    /// derived sibling's `content_type` is often `Plain` because
    /// `ContentType::from_mime` has no closed variant for e.g. `audio/midi`
    /// yet); for a literal-content block, the block's own declared
    /// content_type mime. `None` for a dangling CAS hash — never silently
    /// treated as a match.
    fn block_resolved_mime(&self, snap: &kaijutsu_types::BlockSnapshot) -> Option<String> {
        match self.block_cas_hash(snap) {
            Some(hash) => self
                .kernel()
                .cas()
                .inspect(&hash)
                .ok()
                .flatten()
                .map(|r| r.mime_type),
            None => Some(snap.content_type.as_mime().to_string()),
        }
    }

    /// One-step blob readback (`kj block cat`). Resolves either a named block
    /// or (with `--latest <mime>`) the newest block in `context` whose
    /// resolved mime matches, in timeline order (`block_snapshots` already
    /// returns `blocks_ordered()` — document/timeline order, never the
    /// principal-major `BlockId` order). A CAS-ref block (see
    /// [`Self::block_cas_hash`]) is resolved through CAS; everything else's
    /// `content` IS the payload. Textual payloads (mime starting `text/`)
    /// print directly; binary payloads refuse to dump to the terminal and
    /// require `--out`, mirroring `kj cas get`.
    fn block_cat(
        &self,
        block_id_arg: Option<&str>,
        latest_mime: Option<&str>,
        ctx_ref: Option<&str>,
        out: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let (ctx_id, snap) = match (block_id_arg, latest_mime) {
            (Some(id_str), None) => {
                let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
                    Some(id) => id,
                    None => {
                        return KjResult::Err(format!(
                            "kj block cat: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                        ));
                    }
                };
                let ctx_id = block_id.context_id;
                let snapshots = match self.blocks.block_snapshots(ctx_id) {
                    Ok(s) => s,
                    Err(e) => return KjResult::Err(format!("kj block cat: {e}")),
                };
                let snap = match snapshots.into_iter().find(|b| b.id == block_id) {
                    Some(s) => s,
                    None => {
                        return KjResult::Err(format!(
                            "kj block cat: block '{id_str}' not found in {}",
                            ctx_id.to_hex()
                        ));
                    }
                };
                (ctx_id, snap)
            }
            (None, Some(mime)) => {
                let ctx_id = {
                    let db = self.kernel_db().lock();
                    match resolve_context_arg(ctx_ref, caller, &db) {
                        Ok(id) => id,
                        Err(e) => return KjResult::Err(format!("kj block cat --latest: {e}")),
                    }
                };
                let snapshots = match self.blocks.block_snapshots(ctx_id) {
                    Ok(s) => s,
                    Err(e) => return KjResult::Err(format!("kj block cat --latest: {e}")),
                };
                // Newest-first scan over timeline order (see doc comment);
                // stops at the first match, so a dangling-hash sibling before
                // the match is never resolved needlessly.
                let found = snapshots
                    .into_iter()
                    .rev()
                    .find(|b| self.block_resolved_mime(b).as_deref() == Some(mime));
                let snap = match found {
                    Some(s) => s,
                    None => {
                        return KjResult::Err(format!(
                            "kj block cat --latest: no block with resolved mime '{mime}' in {}",
                            ctx_id.to_hex()
                        ));
                    }
                };
                (ctx_id, snap)
            }
            (Some(_), Some(_)) => {
                // Unreachable in practice — clap's `conflicts_with` rejects
                // this combination before dispatch ever calls this method.
                return KjResult::Err(
                    "kj block cat: a block id and --latest are mutually exclusive".to_string(),
                );
            }
            (None, None) => {
                return KjResult::Err(
                    "kj block cat: provide a block id, or --latest <mime> (with --context)"
                        .to_string(),
                );
            }
        };

        let cas_ref = self.block_cas_hash(&snap);
        let (bytes, mime): (Vec<u8>, String) = match &cas_ref {
            Some(hash) => {
                let cas = self.kernel().cas();
                let info = match cas.inspect(hash) {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj block cat: block references CAS hash {hash} with no metadata \
                             (corruption — refusing to guess a mime)"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj block cat: {e}")),
                };
                let data = match cas.retrieve(hash) {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj block cat: block references CAS hash {hash} but the bytes are \
                             missing (corruption — never a dangling-hash silent skip)"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj block cat: {e}")),
                };
                (data, info.mime_type)
            }
            None => (
                snap.content.clone().into_bytes(),
                snap.content_type.as_mime().to_string(),
            ),
        };
        let is_text = mime.starts_with("text/");
        let block_key = snap.id.to_key();

        if let Some(out_path) = out {
            return match std::fs::write(out_path, &bytes) {
                Ok(()) => {
                    let record = serde_json::json!({
                        "block_id": block_key,
                        "context_id": ctx_id.to_hex(),
                        "cas_hash": cas_ref.as_ref().map(|h| h.to_string()),
                        "mime": mime,
                        "bytes": bytes.len(),
                        "out": out_path,
                    });
                    // Mirrors `kj cas get --out`: the write confirmation is a
                    // human status line, not content the model needs hydrated.
                    KjResult::ok_ephemeral_with_data(
                        format!("wrote {} bytes ({mime}) to {out_path}", bytes.len()),
                        ContentType::Plain,
                        record,
                    )
                }
                Err(e) => KjResult::Err(format!("kj block cat --out: {e}")),
            };
        }

        if !is_text {
            return KjResult::Err(format!(
                "kj block cat: '{block_key}' is binary ({mime}, {} bytes) — refusing to dump to \
                 the terminal; use --out <file>",
                bytes.len(),
            ));
        }

        let record = serde_json::json!({
            "block_id": block_key,
            "context_id": ctx_id.to_hex(),
            "cas_hash": cas_ref.as_ref().map(|h| h.to_string()),
            "mime": mime,
            "bytes": bytes.len(),
        });
        let text = String::from_utf8_lossy(&bytes).into_owned();
        KjResult::ok_typed_with_data(text, ContentType::from_mime(&mime), record)
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

    /// Set a block's status. Mirrors MCP `block_status`. Parses the status
    /// string via `Status::from_str`, which already accepts the lenient set
    /// of synonyms (active→running, completed→done, etc.).
    fn block_status(&self, id_str: &str, new_status: &str) -> KjResult {
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block status: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;
        let status = match Status::from_str(new_status) {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block status: invalid status '{new_status}' (expected pending|running|done|error)"
                ));
            }
        };
        if let Err(e) = self.blocks.set_status(ctx_id, &block_id, status) {
            return KjResult::Err(format!("kj block status: {e}"));
        }
        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "status": status.as_str(),
        });
        KjResult::ok_with_data(
            format!("status set to {}\n", status.as_str()),
            record,
        )
    }

    /// Edit a block via a single line-based operation. Mirrors a single
    /// `EditOp` from MCP `block_edit`. CAS-validated when `--expected` is
    /// provided on Replace; line indices are 0-indexed and half-open.
    fn block_edit(&self, id_str: &str, op: EditOp, caller: &KjCaller) -> KjResult {
        let block_id = match kaijutsu_types::BlockId::from_key(id_str) {
            Some(id) => id,
            None => {
                return KjResult::Err(format!(
                    "kj block edit: malformed id '{id_str}' (expected context_hex_principal_hex_seq)"
                ));
            }
        };
        let ctx_id = block_id.context_id;

        // Fetch current content for offset translation + CAS checks. The op
        // is small; reading the snapshot once is cheap relative to the edit.
        let snap = match self
            .blocks
            .block_snapshots(ctx_id)
            .ok()
            .and_then(|v| v.into_iter().find(|b| b.id == block_id))
        {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block edit: block '{id_str}' not found in {}",
                    ctx_id.to_hex()
                ));
            }
        };
        let content = snap.content.clone();

        // Translate the op into (pos, insert_text, delete_len) and apply.
        let (pos, insert_text, delete_len, op_label) = match op {
            EditOp::Insert { line, content: text } => {
                let pos = match line_to_byte_offset(&content, line) {
                    Ok(p) => p,
                    Err(e) => return KjResult::Err(format!("kj block edit insert: {e}")),
                };
                let text_with_nl = if text.ends_with('\n') || content.is_empty() {
                    text
                } else {
                    format!("{text}\n")
                };
                (pos, text_with_nl, 0usize, "insert")
            }
            EditOp::Delete {
                start_line,
                end_line,
            } => {
                let (start, end) = match line_range_to_byte_range(&content, start_line, end_line) {
                    Ok(pair) => pair,
                    Err(e) => return KjResult::Err(format!("kj block edit delete: {e}")),
                };
                if start >= end {
                    // Empty range — treat as a no-op success rather than an
                    // error. Matches MCP block_edit which silently no-ops here.
                    let record = serde_json::json!({
                        "block_id": id_str,
                        "context_id": ctx_id.to_hex(),
                        "op": "delete",
                        "no_op": true,
                    });
                    return KjResult::ok_with_data("(no-op: empty range)\n".to_string(), record);
                }
                (start, String::new(), end - start, "delete")
            }
            EditOp::Replace {
                start_line,
                end_line,
                content: text,
                expected,
            } => {
                if let Some(ref want) = expected {
                    let actual: String = content
                        .lines()
                        .skip(start_line as usize)
                        .take(end_line.saturating_sub(start_line) as usize)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if actual.trim() != want.trim() {
                        return KjResult::Err(format!(
                            "kj block edit replace: CAS mismatch — expected {want:?} but found {actual:?}"
                        ));
                    }
                }
                let (start, end) = match line_range_to_byte_range(&content, start_line, end_line) {
                    Ok(pair) => pair,
                    Err(e) => return KjResult::Err(format!("kj block edit replace: {e}")),
                };
                let text_with_nl = if text.ends_with('\n') || text.is_empty() {
                    text
                } else {
                    format!("{text}\n")
                };
                (start, text_with_nl, end - start, "replace")
            }
        };

        if let Err(e) = self.blocks.edit_text_as(
            ctx_id,
            &block_id,
            pos,
            &insert_text,
            delete_len,
            Some(caller.principal_id),
        ) {
            return KjResult::Err(format!("kj block edit: {e}"));
        }

        let new_len = self
            .blocks
            .block_snapshots(ctx_id)
            .ok()
            .and_then(|v| v.into_iter().find(|b| b.id == block_id))
            .map(|s| s.content.len())
            .unwrap_or(0);

        let record = serde_json::json!({
            "block_id": id_str,
            "context_id": ctx_id.to_hex(),
            "op": op_label,
            "inserted_bytes": insert_text.len(),
            "deleted_bytes": delete_len,
            "content_length": new_len,
        });
        KjResult::ok_with_data(
            format!(
                "{op_label}: +{}/-{} bytes (total {new_len})\n",
                insert_text.len(),
                delete_len
            ),
            record,
        )
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

/// Compact block handle for `kj block list`: `principal.short()#seq`. The list
/// is scoped to one context, so the block-distinguishing part is enough — and it
/// uses the entropy-tail `short()`, never the shared UUIDv7 timestamp front.
fn short_key(id: &kaijutsu_types::BlockId) -> String {
    format!("{}#{}", id.principal_id.short(), id.seq)
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
            principal_id: PrincipalId::new(),
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

    // ── New: block cat ─────────────────────────────────────────────────

    /// Insert a block explicitly appended after `after` (or, when `after` is
    /// `None`, at the very *start* of the document — `insert_block_as`'s
    /// "no ref given" semantics prepend, they don't append, so a sequence of
    /// bare `after: None` inserts would land in REVERSE order). Tests that
    /// care about timeline order (the `--latest` selector) must thread this
    /// explicitly rather than reuse the order-agnostic `insert_text_block`.
    fn insert_block_ordered(
        d: &crate::kj::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        role: TypesRole,
        content: &str,
        after: Option<kaijutsu_types::BlockId>,
    ) -> kaijutsu_types::BlockId {
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                after.as_ref(),
                role,
                BlockKind::Text,
                content,
                Status::Done,
                ContentType::Plain,
                None,
            )
            .expect("insert_block_as (ordered)")
    }

    /// Store `data` in the dispatcher's CAS under `mime`, then insert an
    /// `Asset`-role block whose content is the resulting hash — the
    /// `img_block`/materialize convention `kj block cat` follows to resolve
    /// CAS-ref blocks (`kaijutsu-hyoushigi::materialize`, `beat.rs`).
    fn insert_cas_asset_block(
        d: &crate::kj::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        data: &[u8],
        mime: &str,
        after: Option<kaijutsu_types::BlockId>,
    ) -> (kaijutsu_types::BlockId, String) {
        let hash = d.kernel().cas().store(data, mime).expect("cas store");
        let id = insert_block_ordered(d, ctx, TypesRole::Asset, hash.as_str(), after);
        (id, hash.to_string())
    }

    #[tokio::test]
    async fn block_cat_reads_literal_text_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, "hello from cat");
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("cat"), bid.to_key()], &c).await;
        assert!(result.is_ok(), "cat failed: {}", result.message());
        assert_eq!(result.message(), "hello from cat");
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert!(v["cas_hash"].is_null(), "literal content has no cas_hash: {v}");
                assert_eq!(v["mime"], "text/plain");
                assert_eq!(v["bytes"], "hello from cat".len());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_cat_resolves_cas_ref_and_prints_textual_payload() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let (bid, hash) =
            insert_cas_asset_block(&d, ctx, b"X:1\nT:Test\nK:C\nC D E F|", "text/vnd.abc", None);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("cat"), bid.to_key()], &c).await;
        assert!(result.is_ok(), "cat failed: {}", result.message());
        // The resolved CAS bytes print — NOT the literal 32-hex hash the block
        // stores as its `content` pointer.
        assert_eq!(result.message(), "X:1\nT:Test\nK:C\nC D E F|");
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["cas_hash"], hash);
                assert_eq!(v["mime"], "text/vnd.abc");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_cat_binary_payload_without_out_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let (bid, _hash) =
            insert_cas_asset_block(&d, ctx, b"MThd fake midi bytes", "audio/midi", None);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("cat"), bid.to_key()], &c).await;
        assert!(!result.is_ok(), "binary cat without --out must refuse: {result:?}");
        assert!(result.message().contains("binary"), "msg: {}", result.message());
        assert!(result.message().contains("audio/midi"), "msg: {}", result.message());
        assert!(result.message().contains("--out"), "msg: {}", result.message());
    }

    #[tokio::test]
    async fn block_cat_binary_payload_with_out_writes_file() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let midi_bytes = b"MThd fake midi bytes".to_vec();
        let (bid, hash) = insert_cas_asset_block(&d, ctx, &midi_bytes, "audio/midi", None);
        let c = caller_with_context(ctx);

        let dir = tempfile::tempdir().expect("tmpdir");
        let out_path = dir.path().join("out.mid");
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("cat"),
                    bid.to_key(),
                    s("--out"),
                    out_path.to_string_lossy().into_owned(),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "cat --out failed: {}", result.message());
        let written = std::fs::read(&out_path).expect("out file written");
        assert_eq!(written, midi_bytes);
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["cas_hash"], hash);
                assert_eq!(v["mime"], "audio/midi");
                assert_eq!(v["bytes"], midi_bytes.len());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_cat_dangling_cas_hash_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        // A well-formed 32-hex hash that was never actually stored in CAS.
        let dangling = kaijutsu_cas::ContentHash::from_data(b"never-stored-bytes").to_string();
        let bid = insert_block_ordered(&d, ctx, TypesRole::Asset, &dangling, None);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("cat"), bid.to_key()], &c).await;
        assert!(!result.is_ok(), "dangling CAS ref must error, not print the bare hash");
        assert!(
            result.message().contains("corruption"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_cat_malformed_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("cat"), s("garbage")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("malformed"));
    }

    #[tokio::test]
    async fn block_cat_missing_block_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        let phantom = kaijutsu_types::BlockId {
            context_id: ctx,
            principal_id: PrincipalId::new(),
            seq: 999,
        };
        let result = d
            .dispatch(&[s("block"), s("cat"), phantom.to_key()], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not found"));
    }

    #[tokio::test]
    async fn block_cat_requires_id_or_latest() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("cat")], &c).await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("--latest"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn block_cat_id_and_latest_conflict() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("cat"),
                    s("some-id"),
                    s("--latest"),
                    s("audio/midi"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "clap must reject id + --latest together");
    }

    #[tokio::test]
    async fn block_cat_latest_selects_newest_matching_mime_in_timeline_order() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        // A deliberately non-BlockId-sorted chain: `insert_block_ordered`
        // threads `after` explicitly so document/timeline order matches
        // insertion order regardless of principal-major BlockId ordering.
        let b1 = insert_block_ordered(&d, ctx, TypesRole::User, "turn one", None);
        let (old_midi, old_hash) =
            insert_cas_asset_block(&d, ctx, b"MThd-old", "audio/midi", Some(b1));
        let b2 = insert_block_ordered(&d, ctx, TypesRole::User, "turn two", Some(old_midi));
        let (new_midi, new_hash) =
            insert_cas_asset_block(&d, ctx, b"MThd-new", "audio/midi", Some(b2));
        assert_ne!(old_hash, new_hash, "fixture sanity: distinct bytes/hashes");

        let dir = tempfile::tempdir().expect("tmpdir");
        let out_path = dir.path().join("latest.mid");
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("cat"),
                    s("--latest"),
                    s("audio/midi"),
                    s("--out"),
                    out_path.to_string_lossy().into_owned(),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "cat --latest failed: {}", result.message());
        let written = std::fs::read(&out_path).expect("out file written");
        assert_eq!(written, b"MThd-new", "must resolve the NEWEST matching block");
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["cas_hash"], new_hash);
                assert_eq!(v["block_id"], new_midi.to_key());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn block_cat_latest_no_match_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        insert_text_block(&d, ctx, "just text, no assets");

        let result = d
            .dispatch(
                &[s("block"), s("cat"), s("--latest"), s("audio/midi")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("audio/midi"),
            "msg: {}",
            result.message()
        );
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

    // ── New: block status ─────────────────────────────────────────────

    #[tokio::test]
    async fn block_status_changes_value() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "x");
        // Confirm starting state is Done (from insert_block_as default).
        let before = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(before.status, Status::Done);

        let result = d
            .dispatch(
                &[s("block"), s("status"), bid.to_key(), s("running")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "status failed: {}", result.message());

        let after = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(after.status, Status::Running);
    }

    #[tokio::test]
    async fn block_status_invalid_value_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);
        let bid = insert_text_block(&d, ctx, "x");

        let result = d
            .dispatch(
                &[s("block"), s("status"), bid.to_key(), s("explosion")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("invalid status"),
            "expected 'invalid status' error: {}",
            result.message()
        );
    }

    // ── New: block edit ───────────────────────────────────────────────

    #[tokio::test]
    async fn block_edit_insert_adds_line() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "first\nthird");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("insert"),
                    s("--line"),
                    s("1"),
                    s("--content"),
                    s("second"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "insert failed: {}", result.message());

        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "first\nsecond\nthird", "got: {:?}", snap.content);
    }

    #[tokio::test]
    async fn block_edit_delete_drops_lines() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        // 4 lines so we have something to drop.
        let bid = insert_text_block(&d, ctx, "a\nb\nc\nd");

        // Delete lines [1, 3) → drops b and c, keeps a and d.
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("delete"),
                    s("--start"),
                    s("1"),
                    s("--end"),
                    s("3"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "delete failed: {}", result.message());
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "a\nd", "got: {:?}", snap.content);
    }

    #[tokio::test]
    async fn block_edit_delete_empty_range_is_noop() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "untouched");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("delete"),
                    s("--start"),
                    s("0"),
                    s("--end"),
                    s("0"),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["no_op"], true, "expected no_op marker: {v}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
        // Content unchanged.
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "untouched");
    }

    #[tokio::test]
    async fn block_edit_replace_swaps_range() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "alpha\nbeta\ngamma");

        // Replace line [1, 2) with "BETA"
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("replace"),
                    s("--start"),
                    s("1"),
                    s("--end"),
                    s("2"),
                    s("--content"),
                    s("BETA"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "replace failed: {}", result.message());
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "alpha\nBETA\ngamma");
    }

    #[tokio::test]
    async fn block_edit_replace_cas_match_succeeds() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "old\nstale\nkeep");

        // --expected matches the current text in the range → swap proceeds.
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("replace"),
                    s("--start"),
                    s("0"),
                    s("--end"),
                    s("2"),
                    s("--content"),
                    s("fresh\nclean"),
                    s("--expected"),
                    s("old\nstale"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "CAS replace failed: {}", result.message());
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "fresh\nclean\nkeep");
    }

    #[tokio::test]
    async fn block_edit_replace_cas_mismatch_rejects() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "actual content");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("replace"),
                    s("--start"),
                    s("0"),
                    s("--end"),
                    s("1"),
                    s("--content"),
                    s("new"),
                    s("--expected"),
                    s("something else entirely"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("CAS mismatch"),
            "expected 'CAS mismatch' error: {}",
            result.message()
        );
        // Block content untouched.
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert_eq!(snap.content, "actual content");
    }

    #[tokio::test]
    async fn block_edit_insert_at_end_appends() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        // 3 lines → valid append positions include line=3 (one past last).
        let bid = insert_text_block(&d, ctx, "a\nb\nc");

        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("insert"),
                    s("--line"),
                    s("3"),
                    s("--content"),
                    s("d"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "append-insert failed: {}", result.message());
        let snap = d
            .block_store()
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .find(|b| b.id == bid)
            .unwrap();
        assert!(snap.content.starts_with("a\nb\nc"), "got: {:?}", snap.content);
        assert!(snap.content.contains('d'), "missing 'd': {:?}", snap.content);
    }

    #[tokio::test]
    async fn block_edit_insert_out_of_range_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let mut c = caller_with_context(ctx);
        c.principal_id = principal;
        let bid = insert_text_block(&d, ctx, "one\ntwo");

        // Block has 2 lines (max addressable insert line = 2). Line 99 is past that.
        let result = d
            .dispatch(
                &[
                    s("block"),
                    s("edit"),
                    bid.to_key(),
                    s("insert"),
                    s("--line"),
                    s("99"),
                    s("--content"),
                    s("nope"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "out-of-range insert should error");
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
