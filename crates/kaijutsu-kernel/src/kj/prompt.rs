//! MCP prompt subcommands: list, show, inject.

use kaijutsu_types::ContentType;

use crate::mcp_pool::serialize_prompt_messages;

use super::parse::{extract_all_named_args, extract_named_arg};
use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_prompt(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.prompt_help());
        }

        match argv[0].as_str() {
            "list" | "ls" => self.prompt_list(argv).await,
            "show" => self.prompt_show(argv).await,
            "inject" => self.prompt_inject(argv, caller).await,
            "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(self.prompt_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj prompt: unknown subcommand '{}'\n\n{}",
                other,
                self.prompt_help()
            )),
        }
    }

    fn prompt_help(&self) -> String {
        include_str!("../../docs/help/kj-prompt.md").to_string()
    }

    /// List prompts from all or a specific MCP server.
    async fn prompt_list(&self, argv: &[String]) -> KjResult {
        let pool = match self.mcp_pool() {
            Some(p) => p,
            None => return KjResult::Err("kj prompt: no MCP pool available".to_string()),
        };

        let server_filter = extract_named_arg(argv, &["--server", "-s"]);

        let servers = match &server_filter {
            Some(name) => vec![name.clone()],
            None => pool.list_servers(),
        };

        if servers.is_empty() {
            return KjResult::ok("(no MCP servers connected)".to_string());
        }

        let mut lines = Vec::new();
        for server_name in &servers {
            match pool.list_prompts(server_name).await {
                Ok(prompts) => {
                    if prompts.is_empty() {
                        lines.push(format!("  {}: (no prompts)", server_name));
                        continue;
                    }
                    for p in &prompts {
                        let args_summary = if p.arguments.is_empty() {
                            String::new()
                        } else {
                            let arg_names: Vec<&str> =
                                p.arguments.iter().map(|a| a.name.as_str()).collect();
                            format!("  ({})", arg_names.join(", "))
                        };
                        let desc = p
                            .description
                            .as_deref()
                            .map(|d| format!("  — {d}"))
                            .unwrap_or_default();
                        lines.push(format!(
                            "  {}/{}{}{}", server_name, p.name, args_summary, desc
                        ));
                    }
                }
                Err(e) => {
                    lines.push(format!("  {}: error: {}", server_name, e));
                }
            }
        }

        KjResult::ok(lines.join("\n"))
    }

    /// Show detailed info about a specific prompt.
    async fn prompt_show(&self, argv: &[String]) -> KjResult {
        let pool = match self.mcp_pool() {
            Some(p) => p,
            None => return KjResult::Err("kj prompt: no MCP pool available".to_string()),
        };

        let (server, name) = match parse_server_prompt(argv.get(1)) {
            Some(pair) => pair,
            None => {
                return KjResult::Err(
                    "kj prompt show: requires <server>/<name>".to_string(),
                )
            }
        };

        match pool.list_prompts(&server).await {
            Ok(prompts) => {
                let prompt = prompts.iter().find(|p| p.name == name);
                match prompt {
                    Some(p) => {
                        let mut lines = vec![format!("Prompt: {}/{}", server, p.name)];
                        if let Some(title) = &p.title {
                            lines.push(format!("Title: {title}"));
                        }
                        if let Some(desc) = &p.description {
                            lines.push(format!("Description: {desc}"));
                        }
                        if p.arguments.is_empty() {
                            lines.push("Arguments: (none)".to_string());
                        } else {
                            lines.push("Arguments:".to_string());
                            for arg in &p.arguments {
                                let req = if arg.required { " (required)" } else { "" };
                                let desc = arg
                                    .description
                                    .as_deref()
                                    .map(|d| format!(" — {d}"))
                                    .unwrap_or_default();
                                lines.push(format!("  {}{}{}", arg.name, req, desc));
                            }
                        }
                        KjResult::ok(lines.join("\n"))
                    }
                    None => KjResult::Err(format!(
                        "kj prompt show: prompt '{}' not found on server '{}'",
                        name, server
                    )),
                }
            }
            Err(e) => KjResult::Err(format!("kj prompt show: {e}")),
        }
    }

    /// Get a prompt and inject its content as a drift block into the current context.
    async fn prompt_inject(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let pool = match self.mcp_pool() {
            Some(p) => p,
            None => return KjResult::Err("kj prompt: no MCP pool available".to_string()),
        };

        let (server, name) = match parse_server_prompt(argv.get(1)) {
            Some(pair) => pair,
            None => {
                return KjResult::Err(
                    "kj prompt inject: requires <server>/<name>".to_string(),
                )
            }
        };

        // Parse --arg key=value pairs
        let arg_values = extract_all_named_args(argv, &["--arg", "-a"]);
        let arguments: Option<std::collections::HashMap<String, String>> = if arg_values.is_empty()
        {
            None
        } else {
            let mut map = std::collections::HashMap::new();
            for kv in &arg_values {
                if let Some((k, v)) = kv.split_once('=') {
                    map.insert(k.to_string(), v.to_string());
                } else {
                    return KjResult::Err(format!(
                        "kj prompt inject: --arg value must be key=value, got '{}'",
                        kv
                    ));
                }
            }
            Some(map)
        };

        // Get the prompt
        let result = match pool.get_prompt(&server, &name, arguments).await {
            Ok(r) => r,
            Err(e) => {
                return KjResult::Err(format!(
                    "kj prompt inject: failed to get prompt '{}/{}': {}",
                    server, name, e
                ))
            }
        };

        // Serialize prompt messages to text
        let content = serialize_prompt_messages(&result.messages);
        let header = format!("[MCP prompt: {}/{}]\n\n", server, name);
        let full_content = format!("{}{}", header, content);

        // Inject as drift block
        use kaijutsu_crdt::DriftKind;
        let after = self.block_store().last_block_id(caller.context_id.unwrap());
        match self.block_store().insert_drift_block(
            caller.context_id.unwrap(),
            None,
            after.as_ref(),
            &full_content,
            caller.context_id.unwrap(),
            Some(format!("mcp:{}", server)),
            DriftKind::Notification,
        ) {
            Ok(_) => KjResult::ok(format!(
                "Injected prompt {}/{} ({} message{})",
                server,
                name,
                result.messages.len(),
                if result.messages.len() == 1 { "" } else { "s" }
            )),
            Err(e) => KjResult::Err(format!("kj prompt inject: failed to insert block: {e}")),
        }
    }
}

/// Parse "server/name" from a single argument.
fn parse_server_prompt(arg: Option<&String>) -> Option<(String, String)> {
    let s = arg?;
    let (server, name) = s.split_once('/')?;
    if server.is_empty() || name.is_empty() {
        return None;
    }
    Some((server.to_string(), name.to_string()))
}
