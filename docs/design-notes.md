# Design Notes

*Collected design explorations and architectural decisions*

---

## Origin: From Rooms to Kernels

The original question: Is the workspace→room hierarchy right?

**Resolution:** Neither. "Room" is the wrong primitive entirely.

The insight came from a philosophical dialogue about AI presence and shared
spaces. For an AI, there's no "waiting room" experience — each prompt IS the
coming-into-being for that moment. The context payload doesn't arrive *to* the
model, it *constitutes* the model for that interaction.

This led to the kernel model: context isn't stored, it's *generated*. The
kernel holds state + mounts. When you need a context payload, kaish walks the
kernel and emits it fresh. Mounts determine what's visible.

**Deprecated terminology:**

| Old | New | Why |
|-----|-----|-----|
| Room | Kernel | Kernel is the primitive. Rooms implied fixed space. |
| Workspace | (removed) | Kernels can mount other kernels. Hierarchy emerges. |
| Join/Leave | Attach/Detach | More accurate to what's happening |

---

## Addressable Entities (@ Routing)

*Status: Design exploration, not yet implemented*

Every entity connected to a kernel gets an `@alias` for direct addressing:

```
> @opus review this architecture
> @haiku summarize in one line
> @bash ls -la
> @amy what do you think?
```

| Type | Examples | Behavior |
|------|----------|----------|
| **model** | `@haiku`, `@sonnet`, `@opus` | Route prompt to LLM |
| **tool** | `@bash`, `@python` | Execute and return result |
| **kernel** | `@project`, `@notes` | Forward to mounted sub-kernel |
| **user** | `@amy`, `@claude` | Mention/notify participant |

**Open questions:**
- Per-kernel or server-global registration?
- Custom aliases? (`@code` → `@sonnet` + coding system prompt)
- Multi-target broadcasting? (`@all`, `@models`)
- Chaining? Can `@opus` invoke `@bash`?

---

## Persistence Architecture

*kaish runs as a separate process, kaijutsu communicates via Unix socket.*

This gives us:
- Process isolation (kaish can be sandboxed, seccomp, namespaced)
- Clean API boundary (Cap'n Proto over Unix socket)
- kaish owns its own state completely
- Crash isolation (kaish crash doesn't kill kaijutsu-server)

```
┌─────────────────────────────────────────────────────────────┐
│  kaijutsu-server                                             │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  KernelState (kaijutsu-side)                            │ │
│  │  ├── state.db: messages, checkpoints, lease, consent    │ │
│  │  └── kaish_socket: /run/user/$UID/kaish/<kernel-id>.sock│ │
│  └─────────────────────────────────────────────────────────┘ │
│                         │ Cap'n Proto / Unix socket          │
└─────────────────────────┼───────────────────────────────────┘
                          │
┌─────────────────────────┼───────────────────────────────────┐
│  kaish process          ▼                                    │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  Kernel                                                 │ │
│  │  ├── Interpreter (scope, eval)                          │ │
│  │  ├── VfsRouter (mounts)                                 │ │
│  │  ├── ToolRegistry (builtins, MCP)                       │ │
│  │  └── state.db (kaish-owned)                             │ │
│  └─────────────────────────────────────────────────────────┘ │
│  Sandboxing: seccomp, namespaces, limited fs access          │
└──────────────────────────────────────────────────────────────┘
```

**Process lifecycle:**
1. User attaches to kernel
2. kaijutsu-server spawns `kaish serve --socket=...` if needed
3. All execute() calls go over the socket
4. On detach: keep running (quick reattach), idle timeout, or archive

**Directory layout:**
```
$XDG_RUNTIME_DIR/kaish/           # Sockets
├── project-x.sock
└── lobby.sock

$XDG_DATA_HOME/kaijutsu/          # State
├── kernels/
│   └── project-x/
│       ├── state.db              # kaijutsu: messages, checkpoints
│       └── kaish.db              # kaish: variables, mounts, tools
└── config.toml
```

---

## Japanese Lexicon Seeds

These alternatives remain interesting for UI/UX flavor:

| Concept | 日本語 | Meaning |
|---------|--------|---------|
| Kernel/Loom | 機 (hata/ki) | Machine, loom, opportunity — we're weaving context |
| Fork/Bud | 芽 (me) | Bud, sprout — emphasizes organic growth |
| Context window | 今 (ima) | Now — it's literally all the model has |
| Emergence | 現れ (araware) | Each interaction is an emergence |
| Substrate | 基層 (kisou) | The embedding space, geometric meaning-landscape |
| Anchor | 錨 (ikari) | Fixed points in context that orient everything |

---

## References

- [kernel-model.md](kernel-model.md) — Authoritative kernel design
- [block-tools.md](block-tools.md) — CRDT block interface
- [diamond-types-fork.md](diamond-types-fork.md) — Why we forked diamond-types
