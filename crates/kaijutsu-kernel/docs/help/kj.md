# kj — kernel command interface

kj runs the kaijutsu kernel from the shell: contexts and their blocks, forks
and drift between contexts, and models.

Get oriented with two reads, one per call: `kj context info` (who you are and
your state) and `kj context list --senpai .` (who answers to you). Read help
only for the verb you are about to use:

- `kj <command> --help` lists a command's subcommands (`kj <command> help` is the same).
- `kj <command> <subcommand> --help` lists its arguments and flags.

A `<ctx>` argument takes `.` (this context, the default), `.parent`, `.senpai`
(the context this one answers to), a chain of those (`.parent.senpai`), a
label, or a hex prefix of an id. Quote ids and labels. A root console has no
senpai.

## Commands

### Contexts

```
context (ctx)   make, list, inspect, switch, rotate, archive contexts; set senpai
fork            fork this context into a child; we stay on the parent
stage           leave blocks out of what models read; commit a staged fork
attach          attach to a context and run its rc attach lifecycle
workspace (ws)  group host paths and bind them to contexts
preset          fork presets: what a child copies from its parent
```

### Work across contexts

```
drift           send text to another context, or distill one into another
wait            wait for a turn, a shell operation, an ask, or a kaish job
watch           watch files; changes go with the next prompt as a notice
drive           start one autonomous turn on a context
ping            wake another context for a turn soon
interrupt       stop a turn without auto-resume
handoff         character handoff log: leave and read notes
board           shared topic boards: list, read, post, reply, search
roster          post your status; list who is around
cc              list live Claude Code sessions; send one a message
```

### Blocks and files

```
block           read, append to, and edit the blocks of a context
shadow          describe an image block; list shadows
search          regex search across blocks
diff            unified diff of a kernel file against disk, another file, or history
vi              kernel-owned vi editor sessions for agents and scripts
vfs             VFS snapshot listings and per-directory activity totals
cas             content-addressed storage: put, get, list, remove
cache           Claude prompt-cache breakpoints on the current context
cp              copy a file between VFS paths
doc             kernel documents of every kind (the storage layer)
swap            keep or discard a recovered file buffer
```

### Models and characters

```
models          list providers, their models, and aliases
synth           synthesize context keywords; inspect the semantic index
kaish           print the kaish agent primer
character       the someone a name resolves to, its model, and its media slots
backend         LLM endpoints and the models on each
alias           short names for a backend and model
embedding       semantic index model
price           what a model's tokens and a provider-billed tool call cost
```

### Music and media

```
transport       a track's beat clock: attach, play, stop, tempo
play            play a sample now, or place it on a track
midi            MIDI device profiles; send raw MIDI to a device
audio           audio node devices, kept MIDI, and beat detection
```

### Kernel administration

```
ledger          answer asks; list standing rules and rc lifecycle runs
binding         a context's loadout: the capabilities it holds
rc              lifecycle scripts under /config/rc
config          read or reset files under /config/kernel and /config/client
system          kernel status and processes; quiesce and resume new turns
council         build this context's council seats before its first gate decision
mcp             external MCP servers: list, reload, restart
hook            the broker's hook tables, edited directly
policy          per-call limits for a registered tool instance
db              back up the kernel database; checkpoint its WAL
```
