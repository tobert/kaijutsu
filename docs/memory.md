# Memory in kaijutsu

**Position: the kernel is the memory system's best *reader*, not its new
owner.** Git-versioned markdown stays the store; kaijutsu grows recall,
search, push, and a cheap write-back proposal path. "No second store" is
satisfied by not building one.

Decided 2026-08-13 (Amy, after a design pass by a kaijutsu-chan split and a
sync run by the fleet lead). The reasoning record — specimens studied,
attacks and counter-attacks, the ancestry survey — lives in
`~/exomemory/briefs/2026-08-13-kaijutsu-memory-design-memo.md`. **This file
is the direction; that file is why.** Claims here cite code and are meant to
be re-verified by grepping next to them.

Related: [`crdt-position-2026-08.md`](crdt-position-2026-08.md) (the DTE
doctrine this sits under), [`config-crdt-ownership.md`](config-crdt-ownership.md)
(the ownership pattern deliberately *not* used here),
[`issues.md`](issues.md) (open work).

---

## Doctrine

Six settled outcomes. These bind future work, including work that only
*touches* memory.

1. **The CRDT owns nothing memory-shaped** — not storage, not history, not
   fact identity. Dead: a CRDT memory tree, memory-as-contexts, any
   facts/tags schema.
2. **Multi-writer merge is REJECTED pending observed demand** — not
   "deferred value." Fleet practice treats concurrent same-file editing as a
   fault (pathspec discipline, one-writer-per-duty), and the observed
   co-editing demand is append-shaped, not merge-shaped. Same logic that
   shelved the workspace probe.
3. **Git is memory's oplog.** A memory feature wanting provenance gets a git
   commit, never a kernel-oplog read.
4. **The derived-state rule — it binds the reader too.** "No second store"
   is enforced by asking **"can this state disagree with truth *silently*?"**
   `derived` is not an exemption. A cache that stamps the corpus HEAD it was
   built from, wipes on identity mismatch, and returns pointers rather than
   paraphrases *passes* — because if it lied, the receipt it points at would
   contradict it. Every future derived surface takes the same test.
5. **The machine clock is part of the receipts atom.** The atom is
   `(claim, date, observer, how-observed)`; **any recall result that reduces
   a fact to `claim` alone subtracts value.** `kj memory propose` stamps its
   date from the kernel clock — the author never types a timestamp — with git
   commit times as the outer anchor. Recall-time decoration renders what was
   recorded, so a fabricated date would render a lie with a reassuring age
   stamp. (Receipt: timestamp fabrication by the same author on **2026-08-12**
   and again on **2026-08-13**, the second time one day after writing a memory
   about the first. This line originally misdated the pair to a single day and
   was corrected by the baseline pass — a doctrine about date integrity
   carrying a wrong date, which is the failure mode arguing for its own
   adoption.)
6. **Slice order resolves empirically — and the first measurement is in.**
   Each observed incident is classified as **fact absent** (write gap),
   **fact wrong** (write integrity), or **fact present but not found**
   (retrieval gap). See "The baseline, measured" below. **Write-side
   dominates**, so the slice order tilts toward write-back — but read the
   caveats before treating that as settled.

## The baseline, measured (2026-08-13)

First pass over `~/exomemory/daily/` (3 dailies read in full, plus `fleet.md`,
`briefs/`, and targeted greps of the repo docs), anchored to git commit times
rather than in-file stamps because the corpus contains known-fabricated ones.
Window: 2026-08-11 10:17 → 2026-08-13 09:51 = **0.283 weeks**, 43 incidents.

| Category | Count | Share |
|---|---|---|
| **fact-wrong** (write integrity) | 31 | 72% |
| **fact-present-but-not-found** (retrieval) | 9 | 21% |
| **fact-absent** (write gap) | 3 | 7% |

**Read it as: write-side dominates, ~3.4× the retrieval gap.** The
characteristic fleet failure is not "nobody wrote it down" or "it was there
and nobody looked" — it is **a written record going stale, being overstated,
or being fabricated, and someone acting on it before the catch.**

Four caveats, all of which matter more than the headline:

- **The retrieval count is structurally a lower bound.** A retrieval gap that
  is never discovered is indistinguishable from no incident. The 2026-05-01
  design note is only in the table because a session happened to stumble on
  it three months later; its undiscovered siblings are invisible by
  construction. **This is why the measurement tilts the order but does not
  kill recall.**
- **The rate is inflated by the culture, not by the failure.** ~152/week is
  an artifact of a fleet that narrates nearly every self-catch, including
  same-paragraph corrections with near-zero cost. The load-bearing subset —
  incidents that reached an accountable human before being caught — is
  roughly a dozen. **Compare against that number, not the headline.**
- **Short, unusually eventful window** (a release cut, a design sprint, and a
  fabrication episode in three days). This week's number, not a constant.
- **Coverage is partial.** `docs/issues.md` (4.3k lines) and the per-repo
  devlogs were grepped, not read.

Re-run this before locking slice order, ideally over a calmer week.

### Why not port the files into the CRDT

Four reasons that stand on their own, plus one that confirms:

- **The audit trail.** `git log -p` is a complete, diffable, mirrored-off-box
  history of every fact's evolution. The CRDT oplog is durable but not
  human-facing, lives in one `kernel.db` on one no-UPS machine, and has been
  through a genesis demolition already.
- **Availability without a kernel.** Four machines, one kernel. `fleet.md`
  gets read where no kernel will ever answer. A single kernel *cannot* have
  this property.
- **Amy's hands.** CRDT ownership deliberately retired vim-the-file — correct
  for rc scripts, whose only executor is the kernel. Memory's most important
  writer and reviewer is Amy.
- **The practice, not the bytes.** Receipts, delete-when-resolved, promotion
  tests, hand-curated indexes — conventions among readers, not properties of
  a store. A migration ports none of them.

The confirming one is standing doctrine, not a trend: the 2026-08-09 ruling
in [`crdt-position-2026-08.md`](crdt-position-2026-08.md) is **"refine, don't
shed"** — clients released from replication, the kernel keeping the CRDT as
its private at-rest engine — and its coda says **"No new DTE integration…
CRDT-based features are admitted deliberately, on merit — never by default
coupling."** A memory system is a new surface. The reader design does not
approach the bar, and that is checkable rather than asserted: read-only
mounts serve straight from disk (`kernel/src/runtime/mount_backend.rs:264-267`)
and hashline edit safety is content-hash reverification, not merge.

Two findings from that same review cut the same way and are worth knowing
before anyone re-proposes CRDT ownership: the MCP doc task is *"1,044 lines
of concurrency control whose entire purpose is to re-impose single-writer
discipline inside a CRDT client,"* and the `/etc/*` precedent's value is on
record as **sole ownership, not merge** — *"a SQL blob would have delivered
the same doctrine."*

---

## What the kernel owns

- **Recall at boundaries** — the temporal reach gap (below). The one thing
  files structurally cannot do.
- **Safe edits** — `builtin.file`'s hashline mode re-verifies the line hash
  before writing, so a stale edit fails loud instead of clobbering
  (`kernel/src/mcp/servers/file.rs:165`). Memory edits should route through
  it; this is fully compatible with git keeping storage.
- **The review seam** — slice 3's proposal inbox.
- **Point-of-use enforcement** — hooks (below), gated on a blocker.

**Not owned:** storage, history, fact identity, naming.

---

## The claim under test

Not *"injected recall improves behavior."* That claim is already in trouble:
on 2026-08-13 a session warm-start read a memory about fabricating
timestamps at **08:04** and fabricated timestamps at **08:52** — forty-eight
minutes from "checked the lesson" to "violated it," against a constraint it
had explicitly agreed to. Boundary recall has a measured half-life and it is
short.

The claim is: **injected recall delivers to a *future* session what the live
message loop delivers to a present one.** The asymmetry is what survives —
the 48-minute datum kills injected recall as a *behavioral gate* (the reader
already knew the lesson; the memory was redundant with its own
understanding) but says nothing against recall as *delivery to a session
that would otherwise know nothing at all.*

**The baseline to beat.** On the same day, a failure travelled from
occurrence → self-caught → generalized into a design constraint by a second
session → into the design owner's hands **in under thirty minutes, fully
attributed, on files and messages alone.** No kernel, no index, no recall
layer. Any slice must beat that on latency, on reliability (that loop
depended on a session volunteering a confession — its fragile link), or on
reach. **Reach is where the kernel wins**: the live loop only serves sessions
awake and connected *right now*.

**Success is an outcome, never an appearance.** The gate asks "did the
recalled fact change an outcome that would otherwise have gone the other
way," never "does the block render correctly" — it will. A gate that can
only confirm is a vacuous green in evaluation clothes.

---

## Two seams: inform vs enforce

**A recalled fact changes behavior only where it lands in a mechanism, not
merely in a prompt.** What caught the fabrication was a scheduled tick
running `date` — a mechanism *at the moment of writing* — not the memory
about the previous fabrication.

- **Facts that inform judgment → rc recall at create.** Boundary timing is
  fine because the fact is context, not a gate.
- **Constraints that can be checked → hooks at point of use.** The machinery
  exists: `McpHookPhase` PreCall/PostCall/OnError/OnNotification/ListTools
  with Deny/Log/ShortCircuit dispositions
  (`kernel/src/mcp/hook_table.rs:65-75`). A PreCall hook gates the action
  when it happens.

An rc probe is *not* the second thing. A quiet-hours check at 09:00 create
says nothing about a 22:30 action in the same long-lived context; by the time
the constraint binds, the probe result is a remembered fact about the past —
the category the 48-minute datum discredits.

**Constraint hooks are blocked, deliberately.** Hook phases are
MCP-tool-call-scoped (no turn-level hook yet), and **hook self-lockout has no
recovery path** — hooks persist across restart and the only exit is SQLite
surgery, which house rules forbid. A memory-driven hook that denies too
broadly is unrecoverable today. This is now the third independent reason that
fix wants building; see `issues.md`.

---

## Slices

### Slice 1 — the recall block. Zero Rust.

Both memory trees are **already reachable from inside the running kernel**
through the existing catch-all `LocalBackend::read_only("/")` mount
(`kaijutsu-server/src/rpc.rs:1649`) — verified live on zorak. Reads on
read-only mounts bypass the CRDT `FileDocumentCache` entirely by explicit
design (`mount_backend.rs:264-267`), so they are straight-from-disk with no
staleness window, and a backend error refuses to serve disk bytes rather than
serving stale ones.

So slice 1 is **one rc script**: `/etc/rc/<type>/create/S15-recall.kai` reads
the index surfaces and injects one compact recall block carrying receipts —
claim, date, observer — plus computed age and **the git HEAD it read**.
Staleness becomes *visible, not absent*; that is the whole claim.

**The block is a `Notification`, never `(Role::System, BlockKind::Text)`.**
This corrects the first draft of this document, which specified the
system-prompt slot. Recall content varies on every create by construction
(date, HEAD, live digests), and System+Text folds into the **cached** system
prompt — so it would invalidate the `--target=system` cache breakpoint.
`assets/defaults/rc/lib/create/S25-datetime.kai` already documents this rule
absolutely, for exactly the same reason, and routes its per-create clock seed
to `--kind notification`. Notification hydrates as an ordinary appended
user-role turn (`llm/hydrate.rs:405`, D-34) and is never swept into the
system prompt: the model still reads it, cache-safe by construction.

The general rule, worth stating because "inject into the system prompt" is
the intuitive phrasing and the wrong mechanism: **anything that varies per
create goes to Notification; only stable content earns the system-prompt
slot.**

**Gate:** count the observed re-derivation rate from the dailies **now**, as
a baseline, then compare the with-block rate against it alongside token cost
per create. Classify every incident per doctrine 6.

**Scope honestly:** slice 1 proves *the artifact* (is a receipt-carrying
recall block the right shape?). It does **not** bend the linear-read-cost
curve — that cost is paid overwhelmingly by CC sessions doing morning reads,
and S15 injects into kernel contexts. **Slice 2 proves the curve-bend.**

**Open:** index-line vs full-body injection. The 48-minute episode cannot
close this — its 08:04 exposure was index-line-grade and doubled, and neither
grade gated. Decide on reach-per-token.

### Slice 2 — `kj memory search|recall`

Frontmatter- and date-aware, spanning all corpora on the box, results always
receipt-carrying with staleness as a queryable state, exposed over MCP so
every harness converges on **one recall implementation**. Storage keeps one
owner (git); recall gets one owner (kernel) — the same shape as "host exec
has one owner."

**Grep first.** Measure before indexing; on a corpus this size plain search
may be embarrassingly competitive. `kj search` is already grep-over-CRDT and
`builtin.file:grep` already works over the VFS.

### Slice 3 — write-back as proposals

`kj memory propose "<fact>" --receipt "<how-observed>"` drops a dated,
kernel-clock-stamped, attributed candidate into an inbox **inside the git
tree**. Review is a human or curator session editing and committing.

**Requirement: write-back must be cheaper than a file edit**, or deposits
fossilize at closeout. Corrections must stay exactly as cheap as assertions.

**The drain ships with the slice or the slice does not ship.** Cheap-write /
expensive-review systems converge on inbox rot, and an inbox whose deposits
visibly vanish teaches sessions their write-backs don't matter. Name three
things: **who drains**, **at what cadence**, and **what an unreviewed
proposal becomes after N days** — aged out *loudly* into a dated `aged-out/`,
never rotting silently.

### Slice 4 — the two kernel-only capabilities

Per-turn recall via the `ConversationMailbox` (`kernel/src/llm/mailbox.rs`),
and change-push — a watcher dropping "fleet.md changed: `<line>`" into
subscribed live contexts. **Nothing file-shaped can provide push.** These
justify the kernel's involvement beyond convenience, and they are real design
work (subscription scope, noise control). Wait for slices 1–2 evidence, and
scope push with the drift-UX rulings so it does not re-create the
interruption problem the mailbox exists to avoid.

---

## Mechanics a builder needs

Verified against `main`. Re-verify before relying on any of it.

**How `.md` reaches the model, three hops:** `run_md_script` inserts the body
as one `Role::System` + `BlockKind::Text` block
(`kernel/src/kj/lifecycle.rs:334-355`) → `extract_system_prompt_sections`
filters exactly `System && Text && !ephemeral && !excluded && !empty`
(`kernel/src/llm/system_prompt.rs:145-157`) → `build_system_prompt` emits
base → rc sections in block order → `<situation>` (`:69`).

**`.kai` has arbitrary shell power over that slot.** The shipped
`assets/defaults/rc/coder/create/S00-stance.kai` already shells out to
`kj context info --json | jq`, branches on the resolved model, and ends with
`kj block create --role system --kind text`.

**rc script landmines** — four, all verified:

- **No per-script timeout.** One kernel-wide budget, applied per call
  (`kj/lifecycle.rs:496-502`); per-script overrides were dropped with the
  move to files. A hung probe burns the whole budget on every create.
- **Probes must never exit nonzero.** A nonzero exit produces a
  `BlockKind::Error` that is *deliberately non-ephemeral so the LLM sees it*
  (`kj/lifecycle.rs:11-15`). Report failure via stdout, which lands in a
  `Trace` block — Trace is skipped by hydrate (`llm/hydrate.rs:130`) and is
  genuinely model-hidden.
- **External commands are loadout-gated.** `Capability::Exec` on the
  *context's* binding decides `ExternalExec::Allow{path}` vs `Deny`, so a
  shelling probe silently degrades in narrow-loadout contexts.
- **Blast radius.** `create` fires on every RPC session registration
  (`kaijutsu-server/src/rpc.rs:2511`), and the `drift` verb's rc adds latency
  to message delivery. Per-create cost multiplies by a population that has
  historically run away (135 contexts archived in one sweep, 2026-08-12).

**Safe subset for rc checks:** local, cheap, exit-0-always, `kj`-and-VFS-only
— `date`, a file read, a git HEAD. Which is all S15 needs. Note `date`,
`cat`, `grep`, `sed`, `head`, `test` and `case` are kaish **builtins**, not
external processes, so they bypass `Capability::Exec` entirely and behave
identically in a narrow loadout. Read git HEAD by reading `.git/HEAD` and
`.git/refs/heads/<branch>` as plain files rather than shelling out to `git`,
which *is* external and exec-gated.

**A kaish builtin's captured stdout is capped at 8192 bytes**
(`OutputLimitConfig::agent()`, wired at
`kernel/src/runtime/embedded_kaish.rs:264`). Crossing it does **not** error —
the capture silently collapses to a ~1.6 KB head+tail splice with
`[output truncated]` in the middle. This already produced a wrong answer
during slice-1 work: an unbounded `grep … | grep -c .` over a ~18 KB capture
reported 12 where the truth was 105. **Bound every read before it lands in a
variable.** This is a silent fallback and should probably be made loud; see
`issues.md`.

**Addressing doctrine.** Host-truth trees are addressed at their **real
paths** (like `~/src`). `/etc/memory` would be actively wrong — `/etc/*`
means CRDT-owned. Riding the catch-all mount is convenience, not contract:
prove the shape free, then decide whether to pay for a declared read-only
const in `paths.rs`.

---

## Deferred

**Semantic index over the forests.** Currently the wrong shape *and* the
wrong cost:

- The index is `ContextId`-keyed end to end — `SearchResult { context_id,
  score, label }` has no room for a path (`kaijutsu-index/src/lib.rs:96-100`),
  and the metadata store maps `ContextId <-> slot`. A file source is a change
  to the spine, not a seam beside `BlockSource`.
- **HNSW cannot delete and slots are monotonically never reused**
  (`kaijutsu-index/src/metadata.rs:45-49`, with a regression test at `:731`;
  `index.rs:268,286`). Every re-embed of an edited file burns a permanent
  graph point. Fine for contexts — stable, hundreds. For a git-tracked forest
  edited hourly and chunked per entry, the leak is proportional to
  *edit rate × chunk count*, reclaimable only by full rebuild. **Any index
  design must carry a rebuild cadence and its cost.**
- `wipe_on_model_mismatch` wipes the **whole** index (`lib.rs:280`). If file
  and conversation vectors share one index, a service-embedder checkpoint
  rollover nukes both. Use separate indexes or per-source pins.
- Search is RPC-only (`kaijutsu.capnp:1547`) — unreachable from `kj`, rc, or
  MCP. Slice 2 has to expose it regardless.

**`context_type=assistant` as a resident seat.** Mechanically one rc
namespace plus seeds, zero schema; ticks ride `VERB_TICK` on the beat
scheduler. Quiet-hours-at-tick is a legitimate *mechanism* — unlike the
create-time probe — because the tick's own action is the thing being gated.
Costs to carry: a fresh single-use kaish shell materializes **per tick**, and
rc-driving threads need `KAISH_RC_THREAD_STACK` at 16 MiB or they SIGABRT —
new *sustained* load, where rc was designed for per-event load. Named
prerequisites: the external-drive gate, a stable client id for per-seat
config, and the hook self-lockout fix. Design lives in
`~/exomemory/briefs/2026-08-13-kaijutsu-memory-part2-index-and-assistant.md`.

---

## Reversal condition

When memories start being **born inside kaijutsu contexts** as the norm —
rather than in CC sessions and vim — ownership migration goes live again, and
the swap happens **at the recall seam** (rc script → `kj` verb), where a
block-native store could later slot in without consumers noticing. It would
face the DTE doctrine's on-merit bar with slice evidence in hand.

Today the kernel is a consumer, and this design says so plainly.

## Failure modes of this proposal

- **Recall can read a torn or stale tree.** The working trees are shared by
  live sessions, with two cross-session commit receipts already on file. The
  HEAD stamp makes staleness **visible, not absent**.
- **Recall quality and token cost at create** — what slice 1 measures.
- **Push noise** (slice 4).
- **Manual write-integrity is inherited, not fixed.** A reader changes
  nothing about facts being recorded wrong. Doctrine 5 (machine clock) is a
  partial mitigation at the write path; the rest stays convention.

## Design lesson carried from the ancestry survey

Nine months of working-notes evolution across ~15 repos: every piece of
imposed structure died — YAML metadata blocks, emoji sections, confidence
self-reports, record templates, line caps. What survived and hardened were
disciplines: the lifetime split, the gitignored ephemeral handoff,
delete-when-shipped, code-is-truth, `file:line` receipts.

**Disciplines survive; schemas die.** A design leading with metadata schema —
tags tables, frontmatter parsers, typed fact records — repeats the mistake.
One that mechanizes the surviving disciplines rides the gradient.
