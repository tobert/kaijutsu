# kaijutsu-mcp

MCP server exposing the Kaijutsu CRDT kernel to MCP clients.

## Usage

```bash
# Run as stdio MCP server (for Claude Code, etc.)
cargo run -p kaijutsu-mcp

# With debug logging
RUST_LOG=debug cargo run -p kaijutsu-mcp
```

### Claude Code Configuration

Add to `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "kaijutsu": {
      "command": "/path/to/kaijutsu-mcp"
    }
  }
}
```

## Tools

### Document Management

| Tool | Description |
|------|-------------|
| `doc_create` | Create a new document (conversation, code, text, or git) |
| `doc_list` | List all documents with metadata and block counts |
| `doc_delete` | Delete a document and all its blocks |

### Block Operations

| Tool | Description |
|------|-------------|
| `block_create` | Create a block with role, kind, and content |
| `block_read` | Read block content with optional line numbers and range |
| `block_append` | Append text to a block (streaming-friendly) |
| `block_edit` | Line-based edit operations (insert, delete, replace) with CAS |
| `block_list` | List blocks with optional filters (document, kind, status, role) |
| `block_status` | Set block status (pending, running, done, error) |

### Search

| Tool | Description |
|------|-------------|
| `kernel_search` | Regex search across blocks with context lines |

### Debug & Visualization

| Tool | Description |
|------|-------------|
| `doc_tree` | Display conversation DAG as ASCII tree |
| `block_inspect` | Dump CRDT internals (version, frontier, metadata) |
| `block_history` | Show block version timeline and creation info |

## Examples

### Visualize Conversation Structure

```
mcp__kaijutsu__doc_tree(document_id: "lobby@main")
```

Output:
```
lobby@main (conversation, 6 blocks)
server/0 [user/text] "write a haiku about haikus"
block_create({) → ✓
server/3 [model/text] "I've written a haiku about haikus!..."
server/4 [user/text] "write me a poem about snow"
server/5 [model/text] "Here's a poem about snow for you:"
```

Tool calls are collapsed by default. Use `expand_tools: true` to see the full DAG:

```
lobby@main (conversation, 6 blocks)
server/0 [user/text] "write a haiku about haikus"
server/1 [model/tool_call] "{"
└─ server/2 [tool/tool_result] "{"block_id":"lobby:default/server/0"..."
server/3 [model/text] "I've written a haiku about haikus!..."
```

### Inspect CRDT State

```
mcp__kaijutsu__block_inspect(block_id: "lobby@main/server/0")
```

Returns:
```json
{
  "block_id": "lobby@main/server/0",
  "version": 6,
  "frontier": [1264],
  "content_length": 26,
  "content_lines": 1,
  "metadata": {
    "role": "user",
    "kind": "text",
    "status": "done",
    "created_at": 1769862548770,
    "author": "server"
  }
}
```

### Block History

```
mcp__kaijutsu__block_history(block_id: "lobby@main/server/3")
```

Output:
```
block: lobby@main/server/3
────────────────────────────────────────
created: 1769862548771ms (unix epoch) by server
version: 6 (document version)
content: 1 line, 223 bytes
status: done
```

## Block Types

| Kind | Role | Description |
|------|------|-------------|
| `text` | user/model/system | Plain text content |
| `thinking` | model | Extended reasoning (collapsible) |
| `tool_call` | model | Tool invocation with JSON input |
| `tool_result` | tool | Tool response (child of tool_call) |

## DAG Structure

Blocks form a DAG via `parent_id` links:

```
user prompt
└─ model thinking
└─ model tool_call
   └─ tool result
└─ model text response
```

The `doc_tree` tool visualizes this structure for debugging.
