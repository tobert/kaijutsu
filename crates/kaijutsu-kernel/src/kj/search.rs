//! `kj search` — regex search across block content.
//!
//! Lifts the MCP `kernel_search` tool into kj. By default searches the
//! current context; `--all` walks every active context registered with
//! the kernel. `--context <ref>` scopes to a single named context the
//! same way `kj block` resolves refs.
//!
//! ```text
//! kj search <pattern> [--context <ref> | --all]
//!                     [--kind <k>] [--role <r>]
//!                     [--context-lines N] [--max-matches N]
//!                     [--json]
//! ```

use clap::Parser;
use kaijutsu_types::{BlockKind, ContentType, ContextId, Role};
use regex::Regex;
use serde::Serialize;

use super::refs::resolve_context_arg;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "search",
    about = "Regex search across block content",
    no_binary_name = true
)]
struct SearchArgs {
    /// Regex pattern (Rust `regex` crate syntax)
    pattern: String,
    /// Single context: . (default) | .parent | <label> | <hex prefix>
    #[arg(long, short = 'c')]
    context: Option<String>,
    /// Search all active contexts instead of just one
    #[arg(long, conflicts_with = "context")]
    all: bool,
    /// Filter by kind: text|thinking|tool_call|tool_result|drift|file|error|notification|resource|trace
    #[arg(long)]
    kind: Option<String>,
    /// Filter by role: user|model|system|tool|asset
    #[arg(long)]
    role: Option<String>,
    /// Lines of context to include before/after each match
    #[arg(long = "context-lines", default_value_t = 2)]
    context_lines: u32,
    /// Maximum number of matches to return
    #[arg(long = "max-matches", default_value_t = 100)]
    max_matches: usize,
    /// Emit a JSON envelope instead of grep-style text
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct SearchMatch {
    context_id: String,
    block_id: String,
    line: u32,
    content: String,
    before: Vec<String>,
    after: Vec<String>,
}

impl KjDispatcher {
    pub(crate) fn dispatch_search(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Empty argv → render help. clap's missing-required-arg message names
        // <pattern>; the explicit empty case keeps the bare invocation friendly.
        if argv.is_empty() {
            let mut cmd = <SearchArgs as clap::CommandFactory>::command();
            return KjResult::ok_ephemeral(cmd.render_help().to_string(), ContentType::Plain);
        }
        let parsed = match SearchArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj search: {e}"));
            }
        };

        let regex = match Regex::new(&parsed.pattern) {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj search: invalid regex: {e}")),
        };

        // Resolve which contexts to walk. `--all` overrides the default;
        // `--context <ref>` resolves through the same path as `kj block`;
        // otherwise scope is just the caller's current context.
        let context_ids: Vec<ContextId> = if parsed.all {
            let db = self.kernel_db().lock();
            match db.list_active_contexts() {
                Ok(rows) => rows.into_iter().map(|r| r.context_id).collect(),
                Err(e) => {
                    return KjResult::Err(format!("kj search: list_active_contexts: {e}"));
                }
            }
        } else {
            let db = self.kernel_db().lock();
            match resolve_context_arg(parsed.context.as_deref(), caller, &db) {
                Ok(id) => vec![id],
                Err(e) => return KjResult::Err(format!("kj search: {e}")),
            }
        };

        let kind_filter = parsed.kind.as_deref().and_then(parse_kind);
        let role_filter = parsed.role.as_deref().and_then(Role::from_str);

        let mut matches: Vec<SearchMatch> = Vec::new();
        let cl = parsed.context_lines as usize;
        let max = parsed.max_matches;

        'outer: for ctx_id in context_ids {
            let snapshots = match self.blocks.block_snapshots(ctx_id) {
                // Missing document or sync error in one context shouldn't abort
                // the whole walk — skip it. Same shape as MCP kernel_search.
                Ok(s) => s,
                Err(_) => continue,
            };

            for snap in snapshots {
                if let Some(k) = kind_filter
                    && snap.kind != k
                {
                    continue;
                }
                if let Some(r) = role_filter
                    && snap.role != r
                {
                    continue;
                }

                let lines: Vec<&str> = snap.content.lines().collect();
                for (idx, line) in lines.iter().enumerate() {
                    if !regex.is_match(line) {
                        continue;
                    }
                    let before: Vec<String> = (0..cl)
                        .filter_map(|i| idx.checked_sub(i + 1).map(|j| lines[j].to_string()))
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let after: Vec<String> = (1..=cl)
                        .filter_map(|i| lines.get(idx + i).map(|s| s.to_string()))
                        .collect();
                    matches.push(SearchMatch {
                        context_id: ctx_id.to_hex(),
                        block_id: snap.id.to_key(),
                        line: idx as u32,
                        content: (*line).to_string(),
                        before,
                        after,
                    });
                    if matches.len() >= max {
                        break 'outer;
                    }
                }
            }
        }

        // Iteration payload: array of block ids, one per match (deduped not
        // worth it — matches are inherently per-line, callers can `sort -u`).
        let id_array = serde_json::Value::Array(
            matches
                .iter()
                .map(|m| serde_json::Value::String(m.block_id.clone()))
                .collect(),
        );

        if parsed.json {
            let envelope = serde_json::json!({
                "matches": matches,
                "total": matches.len(),
                "truncated": matches.len() >= max,
            });
            return KjResult::ok_with_data(envelope.to_string(), id_array);
        }

        // grep-ish text output: each match prefixed with `ctx_short:block_short:line+1`.
        if matches.is_empty() {
            return KjResult::ok_with_data("(no matches)\n".to_string(), id_array);
        }
        let mut out = String::new();
        for m in &matches {
            // 1-indexed line numbers for display; the JSON keeps 0-indexed
            // to round-trip with MCP kernel_search.
            for b in &m.before {
                out.push_str(&format!("  {b}\n"));
            }
            out.push_str(&format!(
                "{ctx_short}:{block_short}:{lineno}:{content}\n",
                ctx_short = &m.context_id[..8.min(m.context_id.len())],
                block_short = short_key(&m.block_id),
                lineno = m.line + 1,
                content = m.content,
            ));
            for a in &m.after {
                out.push_str(&format!("  {a}\n"));
            }
            out.push_str("--\n");
        }
        // Trim the trailing separator so a one-line iteration loop doesn't
        // get phantom blank entries.
        if out.ends_with("--\n") {
            out.truncate(out.len() - 3);
        }
        if matches.len() >= max {
            out.push_str(&format!(
                "(truncated at {max} matches; pass --max-matches to widen)\n"
            ));
        }
        KjResult::ok_with_data(out, id_array)
    }
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

fn short_key(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…", &key[..12])
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{
        BlockKind as TypesBlockKind, ContentType as TypesContentType, ContextId, DocKind,
        PrincipalId, Role as TypesRole, Status as TypesStatus,
    };

    fn s(v: &str) -> String {
        v.to_string()
    }

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

    fn insert_text_block(
        d: &crate::kj::KjDispatcher,
        ctx: ContextId,
        role: TypesRole,
        content: &str,
    ) -> kaijutsu_types::BlockId {
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                role,
                TypesBlockKind::Text,
                content,
                TypesStatus::Done,
                TypesContentType::Plain,
                None,
            )
            .expect("insert_block_as")
    }

    #[tokio::test]
    async fn search_no_matches_returns_friendly_text() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let _ = insert_text_block(&d, ctx, TypesRole::User, "hello world");
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("search"), s("zzzzz")], &c).await;
        assert!(result.is_ok(), "search failed: {}", result.message());
        assert!(
            result.message().contains("no matches"),
            "expected '(no matches)' marker: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn search_finds_line_with_grep_style_output() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let _ = insert_text_block(
            &d,
            ctx,
            TypesRole::User,
            "line one\nfind me here\nline three",
        );
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("search"), s("find me")], &c).await;
        assert!(result.is_ok(), "search failed: {}", result.message());
        let body = result.message();
        assert!(body.contains("find me here"), "match missing: {body}");
        // Context lines included by default (2 before/after); we only have one
        // line on each side here, so both should appear once.
        assert!(body.contains("line one"), "before-context missing: {body}");
        assert!(body.contains("line three"), "after-context missing: {body}");
    }

    #[tokio::test]
    async fn search_json_envelope_shape() {
        use crate::kj::KjResult;
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let bid = insert_text_block(&d, ctx, TypesRole::User, "alpha\nbeta\ngamma");
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(&[s("search"), s("beta"), s("--json")], &c)
            .await;
        assert!(result.is_ok());
        let v: serde_json::Value =
            serde_json::from_str(result.message()).expect("JSON envelope");
        assert_eq!(v["total"], 1);
        assert_eq!(v["truncated"], false);
        let m = &v["matches"][0];
        assert_eq!(m["content"], "beta");
        assert_eq!(m["line"], 1, "0-indexed line in JSON");
        assert_eq!(m["block_id"], bid.to_key());
        assert_eq!(m["context_id"], ctx.to_hex());

        // Iteration data is an array of matched block ids.
        match result {
            KjResult::Ok { data: Some(data), .. } => {
                let arr = data.as_array().expect("data is array");
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0], bid.to_key());
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_kind_filter_excludes_non_matching() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let _text = insert_text_block(&d, ctx, TypesRole::User, "needle");
        // Thinking block also contains "needle" — kind filter should exclude it.
        let _thinking = d
            .block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                TypesRole::Model,
                TypesBlockKind::Thinking,
                "needle in thinking",
                TypesStatus::Done,
                TypesContentType::Plain,
                None,
            )
            .unwrap();
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("search"), s("needle"), s("--kind"), s("text"), s("--json")],
                &c,
            )
            .await;
        assert!(result.is_ok());
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["total"], 1, "kind=text filter must drop thinking: {v}");
    }

    #[tokio::test]
    async fn search_max_matches_truncates() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let _ = insert_text_block(&d, ctx, TypesRole::User, "x\nx\nx\nx\nx");
        let c = caller_with_context(ctx);

        let result = d
            .dispatch(
                &[s("search"), s("x"), s("--max-matches"), s("2"), s("--json")],
                &c,
            )
            .await;
        assert!(result.is_ok());
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["total"], 2);
        assert_eq!(v["truncated"], true);
    }

    #[tokio::test]
    async fn search_invalid_regex_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        // Unbalanced bracket → regex compile failure.
        let result = d.dispatch(&[s("search"), s("[unclosed")], &c).await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("invalid regex"),
            "expected 'invalid regex' message: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn search_context_and_all_conflict() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context_with_doc(&d, Some("c"), principal);
        let c = caller_with_context(ctx);

        // clap's conflicts_with attribute rejects this combo before dispatch.
        let result = d
            .dispatch(
                &[s("search"), s("foo"), s("--all"), s("--context"), s("c")],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().to_lowercase().contains("cannot be used")
                || result.message().to_lowercase().contains("conflict"),
            "expected conflicts error, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn search_all_walks_multiple_contexts() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx_a = register_context_with_doc(&d, Some("a"), principal);
        let ctx_b = register_context_with_doc(&d, Some("b"), principal);
        let _ = insert_text_block(&d, ctx_a, TypesRole::User, "needle in a");
        let _ = insert_text_block(&d, ctx_b, TypesRole::User, "needle in b");
        // Caller is in ctx_a; without --all we'd only see one match.
        let c = caller_with_context(ctx_a);

        let result = d
            .dispatch(&[s("search"), s("needle"), s("--all"), s("--json")], &c)
            .await;
        assert!(result.is_ok(), "search --all failed: {}", result.message());
        let v: serde_json::Value = serde_json::from_str(result.message()).unwrap();
        assert_eq!(v["total"], 2, "both contexts must contribute: {v}");
    }
}
