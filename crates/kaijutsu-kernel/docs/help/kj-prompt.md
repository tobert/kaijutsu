# kj prompt — MCP prompt discovery and injection

Query and inject prompts from connected MCP servers into the current context.

## Subcommands

```
list [--server <name>]          List prompts from all or one server
show <server>/<name>            Show prompt details and arguments
inject <server>/<name> [args]   Get prompt and inject as drift block
```

## Arguments

For `inject`, pass prompt arguments with `--arg key=value` (repeatable):

```bash
kj prompt inject myserver/code_review --arg language=rust --arg style=concise
```

## Examples

```bash
# List all available prompts
kj prompt list

# List prompts from a specific server
kj prompt list --server myserver

# Show details for a prompt
kj prompt show myserver/analyze_code

# Inject a prompt into the current context
kj prompt inject myserver/summarize --arg topic="error handling"
```

## Notes

- Prompt content is injected as a Notification drift block
- The `<server>/<name>` format matches `kj prompt list` output
- Prompt arguments are substituted server-side before injection
