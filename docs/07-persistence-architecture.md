# Persistence Architecture

*Working draft: 2026-01-16*

## Current Thinking

**kaish runs as a separate process, kaijutsu communicates via Unix socket.**

This gives us:
- Process isolation (kaish can be sandboxed, seccomp, namespaced)
- Clean API boundary (Cap'n Proto over Unix socket)
- kaish owns its own state completely
- kaijutsu doesn't need to understand kaish internals
- Crash isolation (kaish crash doesn't kill kaijutsu-server)

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  kaijutsu-server                                                     │
│                                                                      │
│  ┌────────────────────────────────────────────────────────────────┐ │
│  │  KernelState (kaijutsu-side)                                   │ │
│  │  ├── state.db: messages, checkpoints, lease, consent           │ │
│  │  └── kaish_socket: /run/user/$UID/kaish/<kernel-id>.sock       │ │
│  └────────────────────────────────────────────────────────────────┘ │
│                              │                                       │
│                              │ Cap'n Proto / Unix socket             │
│                              │                                       │
└──────────────────────────────┼───────────────────────────────────────┘
                               │
┌──────────────────────────────┼───────────────────────────────────────┐
│  kaish process (sandboxed)   │                                       │
│                              ▼                                       │
│  ┌────────────────────────────────────────────────────────────────┐ │
│  │  Kernel                                                        │ │
│  │  ├── Interpreter (scope, eval)                                 │ │
│  │  ├── VfsRouter (mounts)                                        │ │
│  │  ├── ToolRegistry (builtins, MCP)                              │ │
│  │  ├── JobManager (pipes, background)                            │ │
│  │  └── state.db (kaish-owned)                                    │ │
│  │      ├── variables                                             │ │
│  │      ├── mounts                                                │ │
│  │      ├── tool_config                                           │ │
│  │      ├── job_state                                             │ │
│  │      └── blobs                                                 │ │
│  └────────────────────────────────────────────────────────────────┘ │
│                                                                      │
│  Sandboxing: seccomp, namespaces, limited fs access                  │
└──────────────────────────────────────────────────────────────────────┘
```

## Process Model

```bash
# kaijutsu-server spawns kaish processes as needed
kaijutsu-server
├── listens on TCP 7878 (or SSH)
├── for each kernel:
│   └── spawns: kaish serve --socket=/run/user/$UID/kaish/<id>.sock
│               --state=/var/lib/kaijutsu/kernels/<id>/kaish.db
│               --sandbox
```

**Lifecycle:**
1. User attaches to kernel "project-x"
2. kaijutsu-server checks if kaish process exists for "project-x"
3. If not, spawns `kaish serve --socket=... --state=...`
4. kaijutsu-server connects to socket
5. All execute() calls go over the socket
6. On kernel detach (last user leaves), optionally:
   - Keep kaish running (for quick reattach)
   - Idle timeout → graceful shutdown
   - Or explicit archive → shutdown + compress state

## API Surface

kaish exposes `kaish.capnp::Kernel` over the socket:

```capnp
interface Kernel {
  # Execution
  execute @0 (code :Text) -> (execId :UInt64);
  executeStreaming @1 (code :Text, callback :OutputCallback) -> (execId :UInt64);
  interrupt @2 (execId :UInt64);

  # Variables
  getVar @3 (name :Text) -> (value :Value);
  setVar @4 (name :Text, value :Value);
  listVars @5 () -> (names :List(Text));

  # VFS
  mount @6 (path :Text, source :Text, writable :Bool);
  unmount @7 (path :Text);
  listMounts @8 () -> (mounts :List(MountInfo));

  # Tools
  listTools @9 () -> (tools :List(ToolInfo));
  callTool @10 (name :Text, args :Value) -> (result :ExecResult);

  # MCP
  registerMcp @11 (name :Text, transport :McpTransport);
  listMcpServers @12 () -> (servers :List(McpInfo));

  # State persistence
  snapshot @13 () -> (data :Data);
  restore @14 (data :Data);

  # Blobs
  readBlob @15 (hash :Text) -> (data :Data);
  writeBlob @16 (data :Data) -> (hash :Text);
}
```

## Directory Layout

```
$XDG_RUNTIME_DIR/kaish/           # Sockets (per-user)
├── project-x.sock
├── lobby.sock
└── experiment-42.sock

$XDG_DATA_HOME/kaijutsu/          # kaijutsu state
├── kernels/
│   ├── project-x/
│   │   ├── state.db              # kaijutsu: messages, checkpoints
│   │   └── kaish.db              # kaish: variables, mounts, tools
│   └── lobby/
│       ├── state.db
│       └── kaish.db
└── config.toml

$XDG_DATA_HOME/kaish/             # kaish standalone (not used when embedded)
├── kernels/
│   └── default.db
└── blobs/
```

When kaijutsu spawns kaish, it passes `--state=$XDG_DATA_HOME/kaijutsu/kernels/<id>/kaish.db`, so kaish writes to kaijutsu's directory structure but owns the file contents.

## Sandboxing Options

kaish can be sandboxed since it's a separate process:

| Mechanism | Purpose |
|-----------|---------|
| seccomp | Restrict syscalls (no network except Unix socket, limited fs) |
| namespaces | Isolated PID, mount, user namespaces |
| landlock | Restrict filesystem access to specific paths |
| rlimits | Memory, CPU, file descriptor limits |

```bash
# Example: kaish with restricted fs access
kaish serve \
  --socket=/run/user/1000/kaish/project-x.sock \
  --state=/home/amy/.local/share/kaijutsu/kernels/project-x/kaish.db \
  --sandbox \
  --allow-path=/home/amy/src/project-x:rw \
  --allow-path=/home/amy/src/bevy:ro
```

The `--allow-path` flags configure VFS mount permissions. kaish can only access paths explicitly allowed.

## Checkpoint Flow

```
┌─────────────────────────────────────────────────────────────────┐
│  kaijutsu checkpoint("Implemented auth system")                  │
│                                                                  │
│  1. Query kaish for state via API                                │
│     ├── listVars() → current variables                          │
│     ├── listMounts() → current mounts                           │
│     └── listTools() → registered tools                          │
│                                                                  │
│  2. Get opaque snapshot for restore                              │
│     └── snapshot() → bytes                                      │
│                                                                  │
│  3. Distill message DAG                                          │
│     └── AI or human writes summary                              │
│                                                                  │
│  4. Store in kaijutsu state.db                                   │
│     ├── checkpoint_summary                                       │
│     ├── kaish_state (JSON from API, for context generation)     │
│     ├── kaish_snapshot (bytes, for restore)                     │
│     └── compacted_message_ids                                   │
│                                                                  │
│  5. Compact messages                                             │
│     └── DELETE FROM messages WHERE id IN (compacted_ids)        │
└─────────────────────────────────────────────────────────────────┘
```

## Restore Flow

```
┌─────────────────────────────────────────────────────────────────┐
│  kaijutsu restore(checkpoint_id)                                 │
│                                                                  │
│  1. Load checkpoint from kaijutsu state.db                       │
│                                                                  │
│  2. Spawn new kaish process (or reuse existing)                  │
│     └── kaish serve --socket=... --state=<new-db>               │
│                                                                  │
│  3. Restore kaish state                                          │
│     └── restore(kaish_snapshot)                                 │
│                                                                  │
│  4. Message history shows:                                       │
│     ├── [checkpoint] "Implemented auth system"                  │
│     └── (messages after checkpoint, if any)                     │
└─────────────────────────────────────────────────────────────────┘
```

## Fork/Thread with Separate Processes

**Fork:** New kaish process with copied state
```bash
# Original
kaish serve --socket=.../project-x.sock --state=.../project-x/kaish.db

# Fork creates
cp .../project-x/kaish.db .../experiment/kaish.db
kaish serve --socket=.../experiment.sock --state=.../experiment/kaish.db
```

**Thread:** New kaish process with shared VFS (via mount references)
```bash
# Original has mount: /mnt/src → /home/amy/src/project

# Thread creates new process, copies mount config (not files)
kaish serve --socket=.../parallel.sock --state=.../parallel/kaish.db
# Then: mount("/mnt/src", "/home/amy/src/project", rw=true)
```

Threads share the actual filesystem, not kaish state. Each thread has its own variables, tools, etc.

## Embedded Mode (Optional)

For testing or single-user scenarios, kaish can still be embedded directly:

```rust
// Embedded (no socket, direct calls)
let kaish = kaish_kernel::Kernel::new();
let result = kaish.execute("echo hello").await;

// Socket (separate process)
let kaish = kaish_client::IpcClient::connect("/run/user/1000/kaish/lobby.sock").await?;
let result = kaish.execute("echo hello").await?;
```

Both implement the same trait, so kaijutsu can be configured either way.

## Open Questions

1. **Process lifecycle:** How long does kaish process live after last user detaches?
   - Immediate shutdown?
   - Idle timeout (5 min)?
   - Until explicit archive?

2. **Resource limits:** What are reasonable defaults for sandboxed kaish?
   - Memory: 512MB? 1GB?
   - CPU: 1 core? Unlimited?
   - Open files: 256? 1024?

3. **Blob storage:** Should blobs be shared across kernels?
   - Content-addressed, could dedupe
   - But complicates sandboxing

4. **MCP in sandbox:** How does kaish connect to MCP servers if sandboxed?
   - Socket passthrough from kaijutsu?
   - Explicit allow-list?

5. **Crash recovery:** If kaish crashes mid-execution:
   - kaijutsu should detect, notify user
   - Option to restart kaish, replay from last checkpoint?
   - Or just show error and let user decide?

## Next Steps

1. [ ] kaish L10: Implement state persistence (SQLite)
2. [ ] kaish L11: Implement RPC server (`kaish serve --socket=...`)
3. [ ] kaijutsu: Spawn kaish subprocess instead of embedding
4. [ ] kaijutsu: Implement checkpoint with kaish API queries
5. [ ] Sandboxing: Start with basic rlimits, add seccomp later
