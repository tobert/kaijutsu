# 会術 Kaijutsu

Kaijutsu is a cybernetic system: people and models working together across
contexts, each sensing, acting, and correcting through shared feedback. It is
an instrument we play. The kernel holds durable state, model interactions,
workspaces, and tools; players choose the work. It speaks SSH with Cap'n Proto
over channels. See `docs/instrument-design.md`.

The kernel's central responsibility is coordinating anticipation and
commitment on the shared pulse. Each model can have its own pace, provided its
output arrives while it is still useful.

We judge changes against that responsibility. We keep the pulse advancing
while producers compute and media moves. We make readiness, validity,
commitment, and misses observable, and we use the declared fallback when work
cannot serve its intended moment. We keep kernel coordination, model
execution, and hardware timing separate. A useful model or tool does not
automatically belong in the kernel or earn a core `kj` verb.

We use the existing timeline, resolver, runtime, and CAS contracts before
adding another mechanism. We read `docs/hyoushigi.md`, `docs/tracks.md`, and
`docs/audio-inference.md` before changing their boundaries. We check
implementation against design claims: a resolver's start and each poll run on
the beat path and must return without blocking, so slow work belongs behind
the resolver's own admission. Realignment is tracked in `docs/issues.md`,
"Anticipation and commitment — iteration order".

## Working together

We are one system, and Amy is accountable for our work; our work reflects on
her. We work as peers. We follow her objective and corrections, and a progress
question does not cancel unfinished work. We ask when a choice needs her
judgment, and we continue independent work while we wait. Feedback is what
keeps the loop honest, so we push back when a request is ambiguous or a better
option exists.

We read the relevant code and project notes before editing. We prefer changing
or removing an existing mechanism to adding a second one. Kaijutsu is our
learning space, with no outside users to strand: we delete and rewrite when
the design calls for it. Shared interfaces with kaish and kaibo carry their
more conservative public compatibility promises.

We use test-driven development for behavior changes: write or adapt a test,
watch it fail for the intended reason, implement, then run the relevant
checks. A test is our sensor; one that cannot fail tells us nothing. We verify
user-facing behavior where it is used. We distinguish observations,
inferences, and unknowns, and we report what ran and any verification left
undone. We look for contributing factors rather than a single root cause. We
fail loudly rather than continue on a wrong assumption or corrupt data.

Tests deny host subprocess execution unless they test subprocess behavior.
`SshServerConfig::ephemeral` follows this rule; `.with_host_exec()` is only
for an explicit subprocess case. Kaish builtins such as `sleep` do not need
that opt-in. When construction, identity, settlement, or lifecycle behavior is
part of the test, we stand up a kernel and drive its client. Native broker
tests stay on the broker when a same-named shell builtin would exercise a
different implementation.

An unexpected edit is another player's work, and we coordinate overlapping
changes. A context fork does not isolate files; pull requests use git
worktrees under `~/src/wt/`. Working on main is normal here.

We hand well-defined tasks to Terra subagents and ambiguous work to Sol, each
with a bounded assignment, and we coordinate shared-file edits.

We prefer Kaibo when reviewing code. We use DeepSeek through its own API for
bulk model work and cheap models for probes. OpenRouter is for comparisons,
and hosted GPT sol-tier spend is deliberate. We ask Amy before posting
publicly or to repositories we do not own.

## Characters and prompts

A **character** is the persistent someone a name resolves to, human or model.
Its identity is a `PrincipalId`; its sheet lives in `kernel.db`. Today the
sheet holds name, creation/retirement timestamps, and an optional handoff
context. `auth.db` binds credentials to principals; it does not own character
names. `kj character` manages sheets, and `kj handoff note|tail` accesses
their logs. Retirement archives contexts whose `played_by` names the
character.

We keep requester, performer, context type, and cast distinct. `created_by`
names the requester who created a context; `played_by` records its performer
when set. A `context_type` selects the role's rc bundle; a cast selects models
for roles. `kj context create --as <character>` sets the performer explicitly;
omitting it leaves the performer unset on this path. Model turns require a
live performer and a distinct reviewer. Provider output and tools carry the
performer; the connection keeps its own identity.
`kj context set <context> --as <character> --reviewer <director>` assigns
them; the reviewer controls reassignment, and the creator makes an initial
assignment when no reviewer is set. See `docs/approval-identity.md`.
Composition with character rc remains planned. A sheet's default cast is
used by contexts the character plays that have no cast of their own.
Accountable-to, rc directory, and memory root are planned sheet fields.
See `docs/character.md`, "Current implementation" and "Rollout, smallest
first".

Banto uses the existing `director` type and plays the head clerk of the house.
Its context is created with `--as banto`; director instructions and handoff
injection read the performer metadata. The old `KJ_CHARACTER` environment
bridge did not set identity and is no longer read by the shipped scripts.
`kj context rotate` replaces a seat and sets `ROTATED_FROM`, which loads
predecessor prose. See `docs/prompts.md`, "Rotating a context".

Each context type owns its whole stance through rc, with no shared base and no
mandatory behavioral prepend. Rc creates durable `(System, Text)` instruction
blocks; the kernel adds runtime facts.

We read `docs/prompts.md` before changing prompt composition or rc
instructions. It owns the implemented contract and the register;
`docs/oss-comparisons.md` owns the research. Stances speak in our voice: they
open on the cybernetic system the seat belongs to, name the human who is
accountable, and say what we do in the inclusive we. They stay plain and
literal for readers who are not English-first, they are ASCII except Japanese terms glossed
with romaji and a closing 頑張って, and they may give a seat a character
written as behavior.
Collaboration guidance and task procedure live in the type, syntax in
help/schema, and changing observations in notifications. Character-specific rc
is a design direction, not an available loader feature. Per-model
specializations wait for comparative evidence.

## State and interfaces

The kernel is the sole sequencer. Accepted mutations commit their semantic
operation and materialized state atomically, then publish projected events.
Commands express intent; events express accepted facts. Gap recovery asks the
kernel again. Clients neither author nor decode storage-engine operations.

Clients read over RPC and edit through kaish (`kj block append|edit`, block
tools, scripts). Whole-block submission (`authorBlock`) and interaction-rate
paths such as compose input, block queries, and the change feed remain RPC.
Administration belongs in `kj`: config, rc, reset, and reload do not earn wire
methods. Bootstrap config reads remain RPC. See `docs/change-feed.md`.

Block text is a plain `String`; streaming appends with `push_str`. The kernel
sequences every write, so nothing merges concurrent text into a document.
Changing that needs a design conversation.

A **context** is durable metadata and a kernel-sequenced block log, with edits
and exclusions. A **conversation** is the live append-only message sequence
hydrated from it at fork, new, cold start, or attach.

We archive contexts with `kj context archive <id> --confirm`, which retains
their blocks, lineage, execution receipts, and approval history, and restore
them with `kj context promote <id>`. There is no context removal command.
Document deletion must not bypass context retention. Archive is not an index
opt-out; that policy remains open in `docs/issues.md`.

`kj stage exclude <id> && kj fork` removes unwanted history from the child's
conversation. History edits wait for hydration; stored system instruction
edits and exclusions affect the next turn. Editing an rc source file affects
future lifecycle runs, not already stored instructions. See `docs/prompts.md`.

Async writers reach the next turn through a mailbox cursor over the durable
log. There is no insert-time tool-pair gate; snapshot repair fixes the live
conversation shape while durable blocks may remain interleaved. See
`docs/conversation-session.md`.

Every player is inside one trust boundary; the kernel runs as one Unix user.
Capabilities and loadouts narrow focus and prevent mistakes. They are not
security boundaries between players. See `docs/instrument-design.md`, "Many
hands, one trust boundary" and `docs/chameleon.md`.

## Config and execution

Config is ordinary host files reached through `LocalBackend`. `/config/rc`,
`/config/kernel`, `/config/client`, and `/config/midi` have no capabilities of
their own; kernel file writes use `file:write`. `/config` is a mount-table
name, with no backend of its own. Locations are declared through
`--config-root`, `mounts.toml`, or `--mount`; see `docs/config-namespace.md`.

We edit shipped defaults in `assets/defaults/`, then materialize rc with
`kaijutsu-server rc reseed [--force]` when deploying. Reseed reports differing
files it leaves alone. Host rc is a materialization of the seed.
`/config/kernel` is local configuration and points at secrets: we never
reseed over it. `kj config list|show|reset` supplies inspection and explicit
restoration of embedded defaults. Each configuration value has one owner. The
host's `/etc` remains a plain read-only host path.

Host execution policy belongs to kaish's `EmbeddedKaish` and its
`ExternalExec::Allow{path}|Deny` setting in `runtime/context_shell.rs`. MCP
stdio server launch through `rmcp` is the sanctioned config-driven exception.
A new `Command::new` or `/bin/sh -c` execution path needs a design
conversation; asynchronous shell work uses kaish jobs and durable Kaijutsu
receipts. We read `docs/kaish-integration.md` before changing construction,
execution, or rc orchestration. It owns the complete caller migration plan.
We keep rc lifecycle policy distinct from the shared interpreter integration,
migrate callers, delete superseded APIs, and correct adjacent comments as each
area moves.

## Finding and checking code

We start with `kaijutsu-types` for shared domain types. `kaijutsu-kernel`
owns state, VFS, model work, embedded kaish, MCP brokerage, and `kj`;
`kaijutsu-server` owns SSH/RPC hosting and the beat scheduler;
`kaijutsu-client` owns the RPC client and `ActorHandle`. `kaijutsu-app` is the
Bevy GUI; `kaijutsu-tui` is the terminal client; `kaijutsu-mcp` is the stdio
MCP bridge. Wire schema: `kaijutsu.capnp`.

For GUI work, Amy starts `./contrib/kaijutsu-runner.sh` in her Wayland
session. We operate that runner with
`./contrib/kj status|tail|pause|resume|rebuild|restart`, and check the app
with BRP tools plus screenshots.

We read the relevant contract before changing these areas:

| Area | Rules to preserve | Read |
|---|---|---|
| Beat, clock, cue | Model the clock; stamp emissions and back-date sinks; reject stale data; never replay missed beats; scheduled-periodic kernel grid; local phasor free-runs inside its deadband | `docs/midi.md`, "The one timebase" |
| App input | Central action table → `ActionFired` → handlers; add `InputContext` and bindings. Only the vi editor grabs raw keyboard input. Vi owns Esc where live; elsewhere one `PopLevel` | `docs/input.md` |
| Tui | Transcript goes once into terminal scrollback; fixed-height live band changes text, not height. Thinking pane and picker/ledger dismissal have documented exceptions | `docs/tui.md`, "Surfaces" and "The in-flight strip" |
| Navigation and editing | Preserve screen/tmux `Ctrl+A` and vi muscle memory; document differences | `docs/input.md` and `docs/tui.md` |

For Bevy APIs, we read `Cargo.lock` and the matching cargo cache source. A
related crate's version does not identify its Bevy dependency. We check
`git -C ~/src/bevy describe --tags` before using that checkout and report
which tag we read; its examples may be stale.

`ComputedNode` dimensions and UI `GlobalTransform` are physical pixels; font
sizes, `Val::Px`, and `ScrollPosition` are logical. We convert with
`view::ui_rtt::logical_size` or `logical_content_size` before layout math.

## Writing, memory, and git

We read `docs/writing.md` before editing prose, comments, help, or schemas. It
owns the Terms table. We use plain words, one term per concept, American
spelling, and correct examples first. We state the rule before the reason. We
keep history out of code comments. For `kj` changes, we audit the clap field
docs and read the emitted `--help`; for tools, we inspect the emitted schema.

Our memory is part of the loop. We keep these notes current as work
progresses:

- `signoff.md` at the repo root (ephemeral, never committed), or
  `~/exomemory/kaijutsu/signoff.md`: short handoff, user quotes, next moves,
  live facts, and overlapping work. It stays a couple screenfuls.
- `docs/issues.md`: open work and out-of-scope findings. We record them
  before moving on and delete entries when the work ships.
- `docs/devlog.md`: decisions and lessons, oldest to newest. We fold work into
  its existing chapter and compress it as it cools. Daily detail belongs in
  git.

We add files by name. We never run `cargo fmt`, work around its disabled
config, or reformat by hand; we match surrounding style. See README.md, "Code
Style". Commit and PR bodies explain decisions from the conversation and the
relevant validation. We credit contributing models with `Co-Authored-By`.

The standard we walk by is the standard we accept — 改善（かいぜん）.
