# kj — kernel command interface

Manages contexts, drift, forks, presets, and workspaces in a kaijutsu kernel.

## Context References

Commands accept a context reference wherever `<ctx>` appears:

- `.` — current context (the default when no ref is given)
- `.parent` — the context this was forked from (chainable: `.parent.parent`)
- `explore` — label match (exact, then unique prefix)
- `019c779b` — hex prefix of context UUID

## Common Workflows

### Parallel Exploration

```bash
# Fork two approaches — you stay on the parent (POSIX fork semantics);
# --prompt drives each child's first autonomous turn
kj fork --name approach-a --prompt "try X"
kj fork --name approach-b --prompt "try Y"

# Pull findings back into the parent
kj drift pull approach-a "summarize what you tried"
kj drift pull approach-b "summarize what you tried"
```

### Share a Finding

```bash
# Send a concrete finding (no LLM, fast — delivered immediately)
kj drift push main "retry logic in client.rs:142 drops errors silently"
```

### Complete a Fork

```bash
# Summarize this fork's work back to parent (LLM distillation)
kj drift merge
```

## Commands

```
alias           list, set, remove — short --model handles → backend/model
                (the kernel ships none; they're yours to define)
attach          Attach to an existing context and run its rc attach lifecycle
                (distinct from `transport attach`, which attaches to a beat track)
audio           devices, keep, keep-status, keep-retry, keep-cancel, beats —
                read a connected audio node's device inventory; protect
                recent MIDI in daemon RAM and upload it to CAS (keep), then
                poll/retry/cancel that job; beats is offline audio analysis
                (beat/downbeat tracking via beat-this)
backend         list, show, set, remove, model set|remove, default show|set, reseed —
                SQL-native LLM endpoints (`kernel_db`'s `backends`/`backend_models`/
                `llm_defaults` tables, no TOML, no host file to edit): a
                free-form `name` plus a closed `kind` (anthropic | deepseek |
                openai | codex-app), their context windows, and the
                kernel-wide defaults. No key material is ever stored — only
                an env-var name or a file path.
binding         show, allow, revoke, reset — a context's tool-capability allow-set
                (cap tokens incl. config-write, drive, fork, drift, transport,
                operator, exec, editor, system, admin, or <instance>[:<tool>],
                facade:<name>, *, facade:*)
block           list, inspect, count, read, render, cat, original, reproject,
                append, history, diff, status, create, edit (insert|delete|replace) —
                render engraves a music-notation block to SVG; cat resolves a
                block's payload (following a CAS reference for a derived
                asset) instead of a hand-assembled inspect→cas-get chain;
                original/reproject read an ingest transform's stored raw
                bytes and re-run its style-span parser
cache           list, add, clear — Claude prompt-cache breakpoints on the active context
cas             put, get, ls, info, rm — content-addressed blob storage
cast            list, show, create, remove, set, slot set|remove — named model
                ensembles (role → backend/model + tunables); role is a
                context_type; `set --desc` edits the description after create
                ("" clears it to NULL)
cc              list — roster of live Claude Code sessions on this machine,
                read from ~/.claude/sessions/*.json (never reads *.key files);
                send — deliver a message into a live session's inbox, gated
                behind the approval ledger (`--dry-run` exempt)
character       create, list, show, retire — the sheet a name resolves to
                (principal id + given name); create mints a fresh principal
                and is idempotent on the name; retire concludes and archives
                every live context the character plays
config          Read or reset files under /config/kernel and /config/client.
                Edit them with the file tools or `kj editor`. For model
                configuration, see `kj backend`, `kj cast`, and `kj alias`.
context (ctx)   list, info, prompt, current, switch, create, scratch, rebind,
                set, unset, log, move, rename, archive, conclude, promote,
                demote, pause, resume, remove, retag, hydrate — prompt renders
                a context's system prompt; rebind repairs a context left with
                no usable loadout by re-running `create`'s rc lifecycle,
                ungated (a broken context can always diagnose and repair
                itself, never just abort)
cp              Copy a file between VFS paths via the streaming pump (-r not implemented)
db              backup <path>, checkpoint — hot SQLite backup (VACUUM INTO, absolute
                path required) and WAL checkpoint/quiesce; restore is deliberately NOT
                a verb (see `kj db backup help`)
diff            <a> [<b>] — unified diff as a typed block: one path diffs disk
                against the kernel document that owns it, two paths diff both
                documents, --from/--to address a document's journalled history
doc             list, tree, create, delete — storage layer (all kinds, not just conversation)
drift           push, pull, merge, flush, queue, cancel, history, edge rm
drive           Clock one autonomous turn on a context (--prompt)
editor          open, keys, state, save, quit, list — kernel-owned vi editor sessions
                (save needs the `editor` capability; open/keys/state/quit/list
                are ungated)
fork            Fork current context (--name, --prompt, --preset, --model,
                --include/--exclude ranges, --compact, --as, --stage, --switch)
handoff         note [--for <character>], tail [--window] — a character's
                handoff log (an ordinary context); note is authored by the
                caller even when writing into someone else's log
hook            list, show, remove, add — broker hook tables, direct (never
                through hook evaluation); the recovery path for a self-inflicted
                PreCall Deny("*") lockout
kaish           primer — composed kaish agent-onboarding guidance (kaish-help);
                what S05-kaish.kai turns into a per-context system block
ledger          list, show, allow, deny, rules, forget, runs — answer pending
                approval-ledger asks left by gated verbs (e.g. `kj cc send`);
                allow/deny take `--remember <session|always>` to generalize
                the decision into a standing rule (refused for `allow` when
                a statement has a free variable), `--remember … --family`
                to remember the command family (`kj handoff note`, `git
                push`) whatever the arguments; `rules` lists the rules in
                force for this context with the layer deciding each
                (learned rules, then gate.toml tiers), `forget <rule-id>`
                revokes one; `runs` lists the rc
                lifecycle run log (create/fork/attach/drift/tick/rotate —
                the durable "did the rc lifecycle actually fire" checklist),
                `runs <run-id>` shows one run's per-script detail
mcp             list (alias status), reload — external MCP servers (mcp.toml: kaibo,
                bevy_brp, …); configured-vs-actually-running visibility + reconcile
midi            list, show, send note|cc|pc|sysex, identify, panic — device
                profiles at /config/midi/devices/<name>, ordinary host files
                (docs/midi-next.md); send/panic emit raw MIDI at a named
                device (the kernel never touches hardware — a sink resolves
                the port); identify asks a device what it is and records the
                answer at /run/midi/<device>
model           Show a context's effective model (--context <ref>)
models          List configured providers, their models, and --model aliases
play            Play a sample now (a host path, or --cas <hash> for an object
                already in the CAS), or commit it as a clip cell onto a track
                with --track/--at/--label (docs/pcm.md)
policy          show, set — a registered instance's per-call QoS policy
preset          list, show, save, remove, reseed
rc              add, list, rm, show — lifecycle scripts (/config/rc/<type>/<verb>/).
                They are host files: edit one with `vi <path>` or the file
                tools, and restore the shipped defaults with
                `kaijutsu-server rc reseed`. `rc list` marks each entry
                against its seed (in-sync / differs / not-installed /
                dangling), which the filesystem cannot tell you
roster          status <text> [--availability], list — the live roster: post
                your own self-reported status (identity is always the
                caller's own) or list who's around right now
search          <pattern> — regex search across blocks (--all, --context, --kind, --role)
stage           commit, status, include, exclude — curate a staged (liminal) fork
swap            list, ack, discard — resolve a file buffer recovered after a
                kernel restart (docs/file-buffers.md rule 4); read it first at
                /v/swap/<kernel_id>/<path>
system          status, ps, quiesce [--reason], resume — what the kernel is
                doing right now: status/ps answer "what is running", quiesce
                stops new turns from starting (writes and in-flight turns are
                unaffected; durable across a restart), resume clears it
transport       list, attach, detach, play, pause, stop, tempo <bpm>,
                ooda <on|off>, clock <system|modeled>, rotate, delete — a
                track's beat clock (the musician playhead); list joins the
                durable track table against the live scheduler snapshot, so
                a track in the DB with nothing re-attached this session
                shows as `dormant`
vfs             snapshot <path> (--depth, --max-entries), activity [path] —
                recursive listing + generation stamps / per-directory heat totals
wait            [<target>] [--since, --timeout, --max-blocks, --max-bytes,
                --include text|tools|all] — park until a context's turn
                finishes and report what it produced since you last looked;
                completes the fork/drive/wait delegation quartet. One target
                context at a time; waiting on several delegated children at
                once is unbuilt
workspace (ws)  list, show, create, add, bind, remove
```

Run `kj <command> help` for detailed subcommand reference.
