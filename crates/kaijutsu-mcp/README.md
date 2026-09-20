# kaijutsu-mcp

A stdio MCP bridge to a kaijutsu kernel. It connects over SSH with Cap'n
Proto RPC and exposes a narrow tool surface: an MCP client such as Claude
Code or Codex acts on the kernel mostly through one `shell` tool, which runs
kaish — the same shell `kj` verbs run in. Every player, human or model, works
inside one trust boundary; capabilities narrow focus, they are not a
security control.

## Running it

```bash
kaijutsu-mcp --connect
```

With no `--connect`, the process serves an in-memory local store instead of
a real kernel — useful for exercising the MCP protocol, not for real work.
`--connect` flags, each with an environment-variable fallback a flag wins
over:

| Flag | Default | Purpose |
|---|---|---|
| `--host` | `localhost` | SSH host |
| `--port` | `2222` | SSH port |
| `--kernel` | `lobby` | Kernel ID to attach to |
| `--context-name` | `default` | Context name to join within the kernel |
| `--insecure` | off | Skip known_hosts verification (throwaway kernels only) |
| `--key-fingerprint` | none | `SHA256:…` fingerprint selecting one SSH-agent identity (`KAIJUTSU_KEY_FINGERPRINT`) |
| `--key-file` | none | Unencrypted private key file, read directly (`KAIJUTSU_KEY_FILE`) |
| `--parent` | kernel's only root context | Context (label or id) new sessions are created under (`KAIJUTSU_PARENT`) |
| `--hook-socket` | `$XDG_RUNTIME_DIR/kaijutsu/hook-{ppid}.sock` | Unix socket for hook events |

Naming both `--key-fingerprint` and `--key-file` (after resolving their
variables) is an error. Naming neither tries every key the SSH agent holds,
landing as whichever principal owns the first one the server accepts — the
process warns on startup, since this usually means the bridge is connecting
as your own identity rather than a model character's. Give a character its
own unencrypted key instead; see `docs/character.md`, "The bridge identity:
a key per model character".

### Claude Code and Codex configuration

Add an entry to `~/.claude.json` (user scope) or a project's `.mcp.json`:

```json
{
  "mcpServers": {
    "kaijutsu": { "command": "/path/to/kaijutsu-mcp", "args": ["--connect"] }
  }
}
```

Codex must also forward its thread ID so the process can correlate tools
and hooks with the same session:

```toml
[mcp_servers.kaijutsu]
command = "/path/to/kaijutsu-mcp"
args = ["--connect"]
env_vars = ["CODEX_THREAD_ID", "XDG_RUNTIME_DIR"]
```

`XDG_RUNTIME_DIR` is where the hook listener creates its per-session socket;
without it the tool surface still works, but lifecycle events have no local
transport.

## Tools

| Tool | Does |
|---|---|
| `shell` | Submit a kaish command in the current kernel context. Returns an operation receipt by default; `foreground: true` waits for completion (default timeout 300s, max 600s). Requires `--connect` and a registered session. |
| `register_session` | Register this agent session and join a context. Must run before `shell`. Upserts on the session's label: attaches to an existing live context of that label, or creates a fresh one if the label names a concluded or archived context. |
| `whoami` | This connection's identity: authenticated user, joined context id and label, agent session info. |
| `list_kernel_tools` | List broker tools visible to the joined context (name, description, category, input schema). Requires `--connect`. |
| `invoke_peer` | Call an action on another named RPC peer attached to the kernel (for example `kaijutsu-app`'s `switch_context`), for drift navigation. Requires `--connect`. |

`--connect` mode auto-registers a session context at startup so hook events
land somewhere without a model calling `register_session` first; calling it
manually still works and upserts on the same label.

Example — running a `kj` verb through `shell`:

```json
{"tool": "shell", "arguments": {"command": "kj context list"}}
```

`shell` and the in-kernel `shell` tool return the identical JSON envelope —
`stdout`, `stderr`, `exit_code`, `status`, `data`, `block_id`,
`operation_id`, and more, with an unknown value written as `null`, never
omitted. See `docs/shell-envelope.md` for the full field list and the status
values (`done`, `error`, `rejected`, `running`, `waiting`, `timeout`,
`stream_closed`).

## Prompts and resources

Three MCP prompts read a joined context's blocks directly, independent of
`shell`:

| Prompt | Does |
|---|---|
| `analyze_document` | Structure, content, and activity summary for a context, given its id and a `focus` (`structure`, `content`, `activity`, or `all`). |
| `search_context` | Regex search across a context's blocks (or every joined context), with matching lines and surrounding context. |
| `editing_assistant` | A block's content, its parent block for context, and edit-type instructions (`refine`, `expand`, `summarize`, `fix`), given a block id. |

`list_resources`/`read_resource` expose read-only URIs: `kaijutsu://docs`
(all contexts), `kaijutsu://docs/{context_id}` (one context's metadata and
block list), and `kaijutsu://blocks/{context_id}/{block_key}` (one block's
content).

## The hook adapter

`kaijutsu-mcp hook [claude|codex]` is a one-shot client: it reads one hook
event as JSON on stdin, forwards it to this process's Unix socket listener,
and prints the response. It fails open — a missing socket or unreachable
listener exits 0 rather than blocking the host tool. The listener turns each
event into kernel blocks. See `contrib/claude-hooks.json` and
`contrib/codex-hooks.json` for the configurations that invoke it, and
`docs/cc-peer.md` for the event shapes and the adapters' contract.

## Deployment on zorak

`~/bin/kaijutsu-mcp` is a symlink into `target/debug`; `cargo build -p
kaijutsu-mcp` is the deploy, and every Claude Code session picks up the new
binary on its next `/mcp` reconnect. See `docs/operating.md`, "The MCP
binary".
