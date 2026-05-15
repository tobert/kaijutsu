//! `kj block` — inspect blocks in a context.
//!
//! Wraps the same `BlockStore::block_snapshots` surface that powers the
//! `block_list` / `block_inspect` MCP tools, exposed as kj subcommands so
//! kaish scripts (rc lifecycle, the live-eval harness) can read block state
//! without going through MCP.
//!
//! ```text
//! kj block list   [--context <ref>] [--kind <k>] [--role <r>] [--status <s>] [--json]
//! kj block inspect <block-id> [--json]
//! kj block count  [--context <ref>] [--kind <k>] [--role <r>]
//! ```

use kaijutsu_types::{BlockKind, ContentType, Role, Status};
use serde::Serialize;

use super::parse::extract_named_arg;
use super::refs::resolve_context_arg;
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) fn dispatch_block(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(block_help());
        }
        match argv[0].as_str() {
            "list" | "ls" => self.block_list(&argv[1..], caller),
            "inspect" => self.block_inspect(&argv[1..], caller),
            "count" => self.block_count(&argv[1..], caller),
            "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(block_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj block: unknown subcommand '{}'\n\n{}",
                other,
                block_help()
            )),
        }
    }

    fn block_list(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let json = argv.iter().any(|a| a == "--json");
        let kind_arg = extract_named_arg(argv, &["--kind"]);
        let role_arg = extract_named_arg(argv, &["--role"]);
        let status_arg = extract_named_arg(argv, &["--status"]);
        let ctx_ref = extract_named_arg(argv, &["--context", "-c"]);

        let kernel_id = self.kernel_id();
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx_ref.as_deref(), caller, &db, kernel_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj block list: {e}")),
            }
        };

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block list: {e}")),
        };

        let kf = kind_arg.as_deref().and_then(parse_kind);
        let rf = role_arg.as_deref().and_then(Role::from_str);
        let sf = status_arg.as_deref().and_then(Status::from_str);

        let filtered: Vec<_> = snapshots
            .iter()
            .filter(|b| {
                kf.map_or(true, |k| b.kind == k)
                    && rf.map_or(true, |r| b.role == r)
                    && sf.map_or(true, |s| b.status == s)
            })
            .collect();

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
            return KjResult::ok(out.to_string());
        }

        if filtered.is_empty() {
            return KjResult::ok("(no blocks)".to_string());
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
        KjResult::ok(out)
    }

    fn block_inspect(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        let json = argv.iter().any(|a| a == "--json");
        let id_str = match argv.iter().find(|a| !a.starts_with("--")) {
            Some(s) => s.clone(),
            None => return KjResult::Err("kj block inspect: <block-id> required".into()),
        };

        let parts: Vec<&str> = id_str.split(':').collect();
        if parts.len() != 3 {
            return KjResult::Err(format!(
                "kj block inspect: malformed id '{id_str}' (expected context_id:agent_id:seq)"
            ));
        }
        let ctx_id = match kaijutsu_types::ContextId::parse(parts[0]) {
            Ok(c) => c,
            Err(_) => {
                return KjResult::Err(format!(
                    "kj block inspect: bad context_id in '{id_str}'"
                ));
            }
        };

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block inspect: {e}")),
        };
        let block_count = snapshots.len();
        let snap = match snapshots.iter().find(|b| b.id.to_key() == id_str) {
            Some(s) => s,
            None => {
                return KjResult::Err(format!(
                    "kj block inspect: block '{id_str}' not found in {}",
                    ctx_id.to_hex()
                ));
            }
        };

        if json {
            let out = serde_json::json!({
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
            return KjResult::ok(out.to_string());
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
        KjResult::ok(out)
    }

    fn block_count(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let kind_arg = extract_named_arg(argv, &["--kind"]);
        let role_arg = extract_named_arg(argv, &["--role"]);
        let ctx_ref = extract_named_arg(argv, &["--context", "-c"]);

        let kernel_id = self.kernel_id();
        let ctx_id = {
            let db = self.kernel_db().lock();
            match resolve_context_arg(ctx_ref.as_deref(), caller, &db, kernel_id) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj block count: {e}")),
            }
        };

        let snapshots = match self.blocks.block_snapshots(ctx_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj block count: {e}")),
        };
        let kf = kind_arg.as_deref().and_then(parse_kind);
        let rf = role_arg.as_deref().and_then(Role::from_str);
        let n = snapshots
            .iter()
            .filter(|b| kf.map_or(true, |k| b.kind == k) && rf.map_or(true, |r| b.role == r))
            .count();
        KjResult::ok(n.to_string())
    }
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

fn block_help() -> String {
    "\
kj block — inspect blocks in a context

Commands:
  kj block list   [--context <ref>] [--kind <k>] [--role <r>] [--status <s>] [--json]
  kj block inspect <block-id> [--json]
  kj block count  [--context <ref>] [--kind <k>] [--role <r>]

Filters:
  --kind     text | thinking | tool_call | tool_result | drift | file | error | notification | resource
  --role     user | model | system | tool | asset
  --status   pending | running | done | error
  --context  . (current, default) | .parent | <label> | <hex prefix>

Examples:
  kj block list --json
  kj block list --context main --kind text --role model
  kj block count --kind text
  kj block inspect <ctx>:<agent>:<seq> --json
"
    .to_string()
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
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{DocKind, PrincipalId};

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
        assert!(result.message().contains("nonsense"));
    }

    #[tokio::test]
    async fn block_inspect_missing_id_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("c"), None, principal);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("block"), s("inspect")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("required"));
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
}
