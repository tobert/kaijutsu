//! `kj doc` — manage CRDT documents directly.
//!
//! A document is the storage primitive; a context layers conversation
//! metadata (model binding, system prompt, fork lineage) on top of a
//! Conversation-kind document. Non-conversation kinds (Code, Text,
//! Config) exist as documents only — `kj context list` hides them.
//! This namespace is the kj surface that sees them.
//!
//! ```text
//! kj doc list [--kind <k>] [--json]
//! kj doc tree <id> [--max-depth N] [--expand-tools]
//! kj doc create [--kind <k>] [--language <l>] [--id <hex>]
//! kj doc delete <id> [--confirm <nonce>]
//! ```

use std::str::FromStr;

use clap::{Parser, Subcommand};
use kaijutsu_crdt::{BlockId, BlockKind as CrdtBlockKind, ConversationDAG};
use kaijutsu_types::{ContentType, ContextId, DocKind};
use serde::Serialize;

use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "doc",
    about = "Manage CRDT documents (storage layer)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct DocArgs {
    #[command(subcommand)]
    command: DocCommand,
}

#[derive(Subcommand, Debug)]
enum DocCommand {
    /// List all documents. Includes non-conversation kinds (Code, Text,
    /// Config) that `kj context list` hides. Filter with `--kind`.
    #[command(alias = "ls")]
    List {
        /// Filter by kind: conversation|code|text|config
        #[arg(long)]
        kind: Option<String>,
        /// Emit a JSON object instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Render a document's block DAG as ASCII tree. Most useful for
    /// conversation docs — non-conversation kinds typically have a
    /// single linear chain.
    Tree {
        /// Document id (hex UUID, with or without dashes)
        doc_id: String,
        /// Maximum depth to render (omit for full tree)
        #[arg(long = "max-depth")]
        max_depth: Option<u32>,
        /// Show ToolCall + ToolResult as separate nodes (default: collapsed)
        #[arg(long = "expand-tools")]
        expand_tools: bool,
    },
    /// Create a new document. For Conversation kind, prefer
    /// `kj context create` (which also registers contexts metadata).
    /// Use this verb for Code/Text/Config docs that aren't conversations.
    Create {
        /// Kind: conversation|code|text|config (default: conversation)
        #[arg(long, default_value = "conversation")]
        kind: String,
        /// Programming language (for code documents)
        #[arg(long)]
        language: Option<String>,
        /// Explicit hex UUID to use (omit to generate)
        #[arg(long)]
        id: Option<String>,
    },
    /// Delete a document and all its blocks. CASCADEs to drop the
    /// contexts row, oplog, snapshots — irreversible. Two-step: first
    /// invocation returns a nonce, second invocation with --confirm
    /// performs the deletion (latch pattern shared with archive).
    Delete {
        /// Document id (hex UUID)
        doc_id: String,
        /// Latch confirmation nonce returned by the first invocation
        #[arg(long)]
        confirm: Option<String>,
    },
}

#[derive(Serialize)]
struct DocListRow {
    document_id: String,
    kind: String,
    language: Option<String>,
    /// Block count if the document is resident in the in-memory store.
    /// Documents persisted but not yet hydrated return None.
    block_count: Option<usize>,
    /// Conversation metadata when a contexts row exists (label, model, etc.).
    context: Option<DocContextSummary>,
}

#[derive(Serialize)]
struct DocContextSummary {
    label: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    forked_from: Option<String>,
}

impl KjDispatcher {
    pub(crate) fn dispatch_doc(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            let mut cmd = <DocArgs as clap::CommandFactory>::command();
            return KjResult::ok_ephemeral(cmd.render_help().to_string(), ContentType::Plain);
        }
        let parsed = match DocArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj doc: {e}"));
            }
        };
        // Document lifecycle (create/delete) is operator authority; list/tree read.
        if matches!(parsed.command, DocCommand::Create { .. } | DocCommand::Delete { .. }) {
            if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Operator, "doc") {
                return denied;
            }
        }
        match parsed.command {
            DocCommand::List { kind, json } => self.doc_list(kind.as_deref(), json),
            DocCommand::Tree {
                doc_id,
                max_depth,
                expand_tools,
            } => self.doc_tree(&doc_id, max_depth, expand_tools),
            DocCommand::Create {
                kind,
                language,
                id,
            } => self.doc_create(&kind, language.as_deref(), id.as_deref(), caller),
            DocCommand::Delete { doc_id, confirm } => {
                self.doc_delete(&doc_id, confirm.as_deref(), caller)
            }
        }
    }

    /// List documents from KernelDb (storage of record), join with
    /// BlockStore (memory) for block_count, and KernelDb's contexts
    /// table for label/model when the document is also a context.
    fn doc_list(&self, kind_filter: Option<&str>, json: bool) -> KjResult {
        let kind_p = match kind_filter {
            None => None,
            Some(s) => match DocKind::from_str(s).ok() {
                Some(k) => Some(k),
                None => {
                    // strum::EnumString — fall through if it doesn't parse;
                    // surfaces a friendly error instead of returning nothing.
                    return KjResult::Err(format!(
                        "kj doc list: invalid kind '{s}' (expected conversation|code|text|config)"
                    ));
                }
            },
        };

        let docs = {
            let db = self.kernel_db().lock();
            match kind_p {
                Some(k) => db.list_documents_by_kind(k),
                None => db.list_documents(),
            }
        };
        let docs = match docs {
            Ok(v) => v,
            Err(e) => return KjResult::Err(format!("kj doc list: {e}")),
        };

        // The KV singleton is kernel infrastructure (the kernel-wide key→value
        // store), not a user document. Hide the `kv` kind from the unfiltered
        // listing so `kj doc list` shows what a user authored. An explicit
        // `--kind kv` still surfaces it for introspection.
        let docs: Vec<_> = if kind_p.is_none() {
            docs.into_iter()
                .filter(|d| d.doc_kind != DocKind::Kv)
                .collect()
        } else {
            docs
        };

        // Per-doc context lookup. Each is a single-row PK fetch so an N+1
        // walk is cheap enough for the listing surface; if this grows hot,
        // switch to a LEFT JOIN at the SQL layer.
        let context_meta: Vec<(ContextId, Option<DocContextSummary>)> = {
            let db = self.kernel_db().lock();
            docs.iter()
                .map(|d| {
                    let row = db.get_context(d.document_id).ok().flatten();
                    let summary = row.map(|r| DocContextSummary {
                        label: r.label,
                        provider: r.provider,
                        model: r.model,
                        forked_from: r.forked_from.map(|id| id.to_hex()),
                    });
                    (d.document_id, summary)
                })
                .collect()
        };

        let rows: Vec<DocListRow> = docs
            .iter()
            .zip(context_meta.iter())
            .map(|(d, (_id, ctx))| DocListRow {
                document_id: d.document_id.to_hex(),
                kind: d.doc_kind.as_str().to_string(),
                language: d.language.clone(),
                block_count: self
                    .blocks
                    .get(d.document_id)
                    .map(|e| e.doc.block_count()),
                context: ctx.as_ref().map(|c| DocContextSummary {
                    label: c.label.clone(),
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    forked_from: c.forked_from.clone(),
                }),
            })
            .collect();

        let id_array = serde_json::Value::Array(
            rows.iter()
                .map(|r| serde_json::Value::String(r.document_id.clone()))
                .collect(),
        );

        if json {
            let out = serde_json::json!({
                "count": rows.len(),
                "documents": rows,
            });
            return KjResult::ok_with_data(out.to_string(), id_array);
        }

        if rows.is_empty() {
            return KjResult::ok_with_data("(no documents)\n".to_string(), id_array);
        }
        let mut out = String::new();
        for r in &rows {
            // `<short_id> <kind>[/<lang>] [N blocks] [label="..." model=...]`
            // A UUID document id shows its entropy-tail short(); non-UUID ids
            // (config paths, symlinks) fall back to a leading slice.
            let short = match ContextId::parse(&r.document_id) {
                Ok(c) => c.short(),
                Err(_) => r.document_id.chars().take(12).collect(),
            };
            let kind_part = match &r.language {
                Some(l) => format!("{}/{}", r.kind, l),
                None => r.kind.clone(),
            };
            let bc = match r.block_count {
                Some(n) => format!("{n} blocks"),
                None => "?".to_string(),
            };
            let ctx_part = match &r.context {
                None => String::new(),
                Some(c) => {
                    let mut bits = Vec::new();
                    if let Some(l) = &c.label
                        && !l.is_empty()
                    {
                        bits.push(format!("label={l}"));
                    }
                    if let Some(m) = &c.model
                        && !m.is_empty()
                    {
                        bits.push(format!("model={m}"));
                    }
                    if bits.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", bits.join(" "))
                    }
                }
            };
            out.push_str(&format!("{short}  {kind_part}  ({bc}){ctx_part}\n"));
        }
        KjResult::ok_with_data(out, id_array)
    }

    /// Render the DAG of blocks in `doc_id` as an ASCII tree. Format matches
    /// the MCP doc_tree output so kaish callers can drop in `kj doc tree`
    /// without re-parsing. Collapses ToolCall→ToolResult pairs by default
    /// (matches MCP's expand_tools flag).
    fn doc_tree(&self, id_str: &str, max_depth: Option<u32>, expand_tools: bool) -> KjResult {
        let ctx_id = match ContextId::parse(id_str) {
            Ok(id) => id,
            Err(e) => {
                return KjResult::Err(format!("kj doc tree: invalid doc id '{id_str}': {e}"));
            }
        };

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj doc tree: {e}")),
        };

        let kind_str = self
            .blocks
            .get(ctx_id)
            .map(|e| e.kind.as_str().to_string())
            .unwrap_or_else(|| "conversation".to_string());

        let dag = ConversationDAG::from_snapshots(snapshots);
        let count = dag.len();
        let mut out = format!(
            "{} ({}, {} block{})\n",
            ctx_id.to_hex(),
            kind_str,
            count,
            if count == 1 { "" } else { "s" }
        );

        for (idx, root_id) in dag.roots.iter().enumerate() {
            let is_last_root = idx == dag.roots.len() - 1;
            format_dag_node(
                &dag,
                root_id,
                0,
                "",
                is_last_root,
                max_depth,
                expand_tools,
                &mut out,
            );
        }

        let record = serde_json::json!({
            "document_id": ctx_id.to_hex(),
            "kind": kind_str,
            "block_count": count,
        });
        KjResult::ok_with_data(out, record)
    }

    /// Create a new document. Generates a fresh UUID unless `--id <hex>` is
    /// supplied. For conversation kind, prefer `kj context create` (which
    /// also registers the contexts row) — kj doc create stops at the
    /// storage layer.
    fn doc_create(
        &self,
        kind: &str,
        language: Option<&str>,
        id_arg: Option<&str>,
        _caller: &KjCaller,
    ) -> KjResult {
        let kind_p = match DocKind::from_str(kind).ok() {
            Some(k) => k,
            None => {
                return KjResult::Err(format!(
                    "kj doc create: invalid kind '{kind}' (expected conversation|code|text|config)"
                ));
            }
        };
        let new_id = match id_arg {
            None => ContextId::new(),
            Some(hex) => match ContextId::parse(hex) {
                Ok(id) => id,
                Err(e) => {
                    return KjResult::Err(format!(
                        "kj doc create: invalid --id '{hex}': {e}"
                    ));
                }
            },
        };
        if let Err(e) = self
            .blocks
            .create_document(new_id, kind_p, language.map(|s| s.to_string()))
        {
            return KjResult::Err(format!("kj doc create: {e}"));
        }
        let record = serde_json::json!({
            "document_id": new_id.to_hex(),
            "kind": kind_p.as_str(),
            "language": language,
        });
        KjResult::ok_with_data(
            format!("{}\n", new_id.to_hex()),
            serde_json::Value::Array(vec![serde_json::Value::String(new_id.to_hex())]),
        )
        // The detailed record isn't currently exposed via `data` (kept as
        // an array of one id for iteration parity with `kj block create`).
        // If callers need the full record, switch this surface to the
        // {data, record} envelope we discussed elsewhere.
        .preserving_record(record)
    }

    /// Delete a document and CASCADE-drop its contexts row, oplog,
    /// snapshots. Latch-gated: first call returns a nonce + summary,
    /// second call with `--confirm <nonce>` actually deletes.
    fn doc_delete(
        &self,
        id_str: &str,
        confirm: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let ctx_id = match ContextId::parse(id_str) {
            Ok(id) => id,
            Err(e) => {
                return KjResult::Err(format!(
                    "kj doc delete: invalid doc id '{id_str}': {e}"
                ));
            }
        };

        // Reject inputs that don't exist before asking for confirmation —
        // the latch message would otherwise be meaningless.
        let (kind_str, block_count) = {
            let db = self.kernel_db().lock();
            match db.get_document(ctx_id) {
                Ok(Some(row)) => {
                    let bc = self
                        .blocks
                        .get(ctx_id)
                        .map(|e| e.doc.block_count())
                        .unwrap_or(0);
                    (row.doc_kind.as_str().to_string(), bc)
                }
                Ok(None) => {
                    return KjResult::Err(format!(
                        "kj doc delete: doc '{id_str}' not found"
                    ));
                }
                Err(e) => return KjResult::Err(format!("kj doc delete: {e}")),
            }
        };

        // Without --confirm: emit a Latch so the kaish layer prints the
        // confirmation summary and a nonce. With --confirm: caller.confirmed
        // is set true by KjBuiltin if the nonce verifies; otherwise we'd
        // never reach here with confirm set, so trust it.
        if !caller.confirmed && confirm.is_none() {
            return KjResult::Latch {
                command: "kj doc delete".to_string(),
                target: id_str.to_string(),
                message: format!(
                    "{} doc ({block_count} blocks) — cascade-drops contexts/oplog/snapshots",
                    kind_str
                ),
            };
        }

        if let Err(e) = self.blocks.delete_document(ctx_id) {
            return KjResult::Err(format!("kj doc delete: {e}"));
        }
        let record = serde_json::json!({
            "document_id": ctx_id.to_hex(),
            "kind": kind_str,
            "deleted_blocks": block_count,
        });
        KjResult::ok_with_data(format!("deleted {}\n", id_str), record)
    }
}

/// Carries a structured `record` JSON alongside the iteration-friendly
/// `data` field. Placeholder — currently a no-op (see comment in
/// `doc_create`); kept so call sites encoding the intent are explicit
/// when we later promote the inner shape. Marked private so it doesn't
/// leak into the wider KjResult surface.
trait PreservingRecord {
    fn preserving_record(self, _record: serde_json::Value) -> Self;
}

impl PreservingRecord for KjResult {
    fn preserving_record(self, _record: serde_json::Value) -> Self {
        self
    }
}

// ── DAG tree formatter (port of kaijutsu-mcp/src/tree.rs) ────────────
//
// Inlined rather than imported so kj doesn't depend on kaijutsu-mcp.
// When we factor out a shared tree-format helper (likely into
// kaijutsu-crdt where ConversationDAG lives), both consumers swap to it.

fn format_dag_node(
    dag: &ConversationDAG,
    block_id: &BlockId,
    depth: usize,
    prefix: &str,
    is_last: bool,
    max_depth: Option<u32>,
    expand_tools: bool,
    out: &mut String,
) {
    if let Some(max) = max_depth
        && depth as u32 > max
    {
        return;
    }
    let block = match dag.get(block_id) {
        Some(b) => b,
        None => return,
    };

    let connector = if depth == 0 {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };

    let short_id = format!("{}:{}", block_id.principal_id.short(), block_id.seq);
    let role_kind = format!("[{}/{}]", block.role.as_str(), block.kind.as_str());
    let summary = summarize(&block.content, 40);

    let children = dag.get_children(block_id);
    let can_collapse = !expand_tools
        && block.kind == CrdtBlockKind::ToolCall
        && children.len() == 1
        && dag
            .get(&children[0])
            .is_some_and(|c| c.kind == CrdtBlockKind::ToolResult);

    if can_collapse {
        let result_block = dag.get(&children[0]).unwrap();
        let tool_name = block.tool_name.as_deref().unwrap_or("tool");
        let status_icon = if result_block.is_error { "✗" } else { "✓" };
        out.push_str(&format!(
            "{prefix}{connector}{tool_name}({summary}) → {status_icon}\n"
        ));
        return;
    }

    out.push_str(&format!(
        "{prefix}{connector}{short_id} {role_kind} \"{summary}\"\n"
    ));

    let child_prefix = if depth == 0 {
        "".to_string()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };
    for (i, child_id) in children.iter().enumerate() {
        let is_last_child = i == children.len() - 1;
        format_dag_node(
            dag,
            child_id,
            depth + 1,
            &child_prefix,
            is_last_child,
            max_depth,
            expand_tools,
            out,
        );
    }
}

fn summarize(content: &str, max_chars: usize) -> String {
    let first = content.lines().next().unwrap_or("").trim();
    if first.chars().count() <= max_chars {
        first.to_string()
    } else {
        let truncated: String = first.chars().take(max_chars - 3).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::KjResult;
    use kaijutsu_types::{
        BlockKind as TypesBlockKind, ContentType as TypesContentType, ContextId, DocKind,
        PrincipalId, Role as TypesRole, Status as TypesStatus,
    };

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// Register a context AND its document — matches production
    /// `kj context create` behavior. `register_context` already writes
    /// the documents + contexts rows to KernelDb (the storage of record
    /// `kj doc list` reads from); `block_store.create_document` adds the
    /// in-memory store entry that backs block_count + block snapshots.
    fn register_context_with_doc(
        d: &crate::kj::KjDispatcher,
        label: Option<&str>,
        principal: PrincipalId,
    ) -> ContextId {
        let ctx = register_context(d, label, None, principal);
        d.block_store()
            .create_document(ctx, DocKind::Conversation, None)
            .expect("create_document");
        ctx
    }

    /// Insert a non-conversation document directly into KernelDb (matching
    /// what `block_store.create_document` would do when wired to a DB,
    /// which the test fixture doesn't do). Also hydrates the in-memory
    /// store so block_count is non-None in listings.
    fn register_doc_in_db(
        d: &crate::kj::KjDispatcher,
        id: ContextId,
        kind: DocKind,
        language: Option<&str>,
        principal: PrincipalId,
    ) {
        use crate::kernel_db::DocumentRow;
        let db = d.kernel_db().lock();
        let ws_id = db
            .get_or_create_default_workspace(principal)
            .expect("default workspace");
        db.insert_document(&DocumentRow {
            document_id: id,
            workspace_id: ws_id,
            doc_kind: kind,
            language: language.map(|s| s.to_string()),
            path: None,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: principal,
        })
        .expect("insert_document");
        drop(db);
        d.block_store()
            .create_document(id, kind, language.map(|s| s.to_string()))
            .expect("create_document in store");
    }

    fn insert_text_block(
        d: &crate::kj::KjDispatcher,
        ctx: ContextId,
        content: &str,
    ) -> kaijutsu_types::BlockId {
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                TypesRole::User,
                TypesBlockKind::Text,
                content,
                TypesStatus::Done,
                TypesContentType::Plain,
                None,
            )
            .expect("insert_block_as")
    }

    // ── doc list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn doc_list_empty_returns_friendly_text() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[s("doc"), s("list")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());
        assert!(
            result.message().contains("no documents"),
            "got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn doc_list_includes_non_conversation_docs() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();

        // Create three docs: one Conversation (with context), one Code, one Config.
        let conv_ctx = register_context_with_doc(&d, Some("chat"), principal);
        let code_id = ContextId::new();
        register_doc_in_db(&d, code_id, DocKind::Code, Some("rust"), principal);
        let cfg_id = ContextId::new();
        register_doc_in_db(&d, cfg_id, DocKind::Config, None, principal);

        let c = caller_with_context(conv_ctx);
        let result = d.dispatch(&[s("doc"), s("list"), s("--json")], &c).await;
        assert!(result.is_ok(), "list failed: {}", result.message());

        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["count"], 3, "must include non-conversation docs: {v}");

        // The kind filter is honored; code-only returns the code doc.
        let result = d
            .dispatch(
                &[s("doc"), s("list"), s("--kind"), s("code"), s("--json")],
                &c,
            )
            .await;
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["documents"][0]["kind"], "code");
        assert_eq!(v["documents"][0]["language"], "rust");
    }

    #[tokio::test]
    async fn doc_list_attaches_context_metadata() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let conv = register_context_with_doc(&d, Some("named-conversation"), principal);
        let c = caller_with_context(conv);

        let result = d.dispatch(&[s("doc"), s("list"), s("--json")], &c).await;
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        let doc = &v["documents"][0];
        assert_eq!(doc["context"]["label"], "named-conversation");
    }

    #[tokio::test]
    async fn doc_list_ls_alias() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let conv = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(conv);

        let result = d.dispatch(&[s("doc"), s("ls"), s("--json")], &c).await;
        assert!(result.is_ok());
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["count"], 1);
    }

    #[tokio::test]
    async fn doc_list_invalid_kind_errors() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(&[s("doc"), s("list"), s("--kind"), s("bogus")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("invalid kind"));
    }

    #[tokio::test]
    async fn doc_list_data_is_iterable_array() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let _conv = register_context_with_doc(&d, Some("c"), principal);
        let c = test_caller();

        let result = d.dispatch(&[s("doc"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("data must be array");
                assert_eq!(arr.len(), 1);
                assert!(arr[0].is_string());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn doc_list_hides_kv_singleton_by_default_but_kind_kv_surfaces_it() {
        // `test_dispatcher` wires `init_kv`, which mints the kernel KV singleton
        // (kind `kv`). It's infrastructure, not a user document, so the
        // unfiltered listing must not count it — but `--kind kv` is the explicit
        // escape hatch for introspection.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let _conv = register_context_with_doc(&d, Some("chat"), principal);
        let c = test_caller();

        // Unfiltered: exactly the one user conversation, KV singleton hidden.
        let result = d.dispatch(&[s("doc"), s("list"), s("--json")], &c).await;
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["count"], 1, "kv singleton must be hidden by default: {v}");
        let kinds: Vec<&str> = v["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["kind"].as_str().unwrap())
            .collect();
        assert!(!kinds.contains(&"kv"), "unfiltered list leaked kv: {kinds:?}");

        // Explicit `--kind kv` surfaces it.
        let result = d
            .dispatch(&[s("doc"), s("list"), s("--kind"), s("kv"), s("--json")], &c)
            .await;
        assert!(result.is_ok(), "kind kv failed: {}", result.message());
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["count"], 1, "explicit --kind kv must surface it: {v}");
        assert_eq!(v["documents"][0]["kind"], "kv");
    }

    // ── doc tree ───────────────────────────────────────────────────

    #[tokio::test]
    async fn doc_tree_empty_document_renders_header_only() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let conv = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(conv);

        let result = d
            .dispatch(&[s("doc"), s("tree"), conv.to_hex()], &c)
            .await;
        assert!(result.is_ok(), "tree failed: {}", result.message());
        let body = result.message();
        assert!(body.contains("(conversation, 0 blocks)"), "got: {body}");
    }

    #[tokio::test]
    async fn doc_tree_with_blocks_renders_root() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let conv = register_context_with_doc(&d, Some("c"), principal);
        let _ = insert_text_block(&d, conv, "first message");
        let c = caller_with_context(conv);

        let result = d
            .dispatch(&[s("doc"), s("tree"), conv.to_hex()], &c)
            .await;
        assert!(result.is_ok(), "tree failed: {}", result.message());
        let body = result.message();
        assert!(body.contains("[user/text]"), "missing role/kind: {body}");
        assert!(body.contains("first message"), "missing content: {body}");
    }

    #[tokio::test]
    async fn doc_tree_invalid_id_errors() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[s("doc"), s("tree"), s("not-hex")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("invalid doc id"));
    }

    // ── doc create ─────────────────────────────────────────────────

    #[tokio::test]
    async fn doc_create_default_kind_is_conversation() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[s("doc"), s("create")], &c).await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let id_hex = result.message().trim();
        let id = ContextId::parse(id_hex).expect("returned id parseable");
        let entry = d.block_store().get(id).expect("doc resident in store");
        assert_eq!(entry.kind, DocKind::Conversation);
    }

    #[tokio::test]
    async fn doc_create_code_with_language() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(
                &[
                    s("doc"),
                    s("create"),
                    s("--kind"),
                    s("code"),
                    s("--language"),
                    s("rust"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok());
        let id = ContextId::parse(result.message().trim()).unwrap();
        let entry = d.block_store().get(id).unwrap();
        assert_eq!(entry.kind, DocKind::Code);
        assert_eq!(entry.language.as_deref(), Some("rust"));
    }

    #[tokio::test]
    async fn doc_create_with_explicit_id() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let chosen = ContextId::new();

        let result = d
            .dispatch(
                &[s("doc"), s("create"), s("--id"), chosen.to_hex()],
                &c,
            )
            .await;
        assert!(result.is_ok(), "create with --id failed: {}", result.message());
        assert_eq!(result.message().trim(), chosen.to_hex());
        assert!(d.block_store().get(chosen).is_some());
    }

    #[tokio::test]
    async fn doc_create_invalid_kind_errors() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d
            .dispatch(&[s("doc"), s("create"), s("--kind"), s("not-a-kind")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("invalid kind"));
    }

    // ── doc delete ─────────────────────────────────────────────────

    #[tokio::test]
    async fn doc_delete_without_confirm_returns_latch() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let conv = register_context_with_doc(&d, Some("c"), principal);
        let _ = insert_text_block(&d, conv, "x");
        let c = caller_with_context(conv);

        let result = d
            .dispatch(&[s("doc"), s("delete"), conv.to_hex()], &c)
            .await;
        assert!(result.is_latch(), "expected Latch, got {result:?}");
        match result {
            KjResult::Latch { command, target, .. } => {
                assert_eq!(command, "kj doc delete");
                assert_eq!(target, conv.to_hex());
            }
            other => panic!("expected Latch, got {other:?}"),
        }
        // Doc still present in store.
        assert!(d.block_store().get(conv).is_some());
    }

    #[tokio::test]
    async fn doc_delete_with_confirmed_caller_succeeds() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let conv = register_context_with_doc(&d, Some("c"), principal);
        let _ = insert_text_block(&d, conv, "x");
        let c = confirmed_caller(conv);

        let result = d
            .dispatch(&[s("doc"), s("delete"), conv.to_hex()], &c)
            .await;
        assert!(result.is_ok(), "delete failed: {}", result.message());
        // Memory store dropped.
        assert!(d.block_store().get(conv).is_none());
    }

    #[tokio::test]
    async fn doc_delete_unknown_doc_errors() {
        let d = test_dispatcher().await;
        let c = confirmed_caller(ContextId::new());

        let result = d
            .dispatch(&[s("doc"), s("delete"), ContextId::new().to_hex()], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("not found"));
    }

    #[tokio::test]
    async fn doc_delete_invalid_id_errors() {
        let d = test_dispatcher().await;
        let c = confirmed_caller(ContextId::new());

        let result = d
            .dispatch(&[s("doc"), s("delete"), s("garbage")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("invalid doc id"));
    }
}
