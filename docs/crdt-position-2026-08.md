# CRDT Position Review — 2026-08-09

Provenance: Amy asked *"how much are CRDTs really helping us... the kernel
already mediates a lot... I just want to make sure it's really serving
us."* Evidence dossier by kaibo (gemini explorer, cited file:line);
deliberation by a Claude Fable 5 subagent with read-only repo access that
spot-checked every load-bearing claim. **Decision: PENDING Amy.** The paper
below is verbatim from the deliberation.

---

# Position Paper: Is the CRDT Investment Serving Kaijutsu?

*Deliberation over the gemini survey dossier, spot-checked against main (2026-08-09). Everything cited below was verified by reading the code, not taken from the dossier on faith.*

## Summary position

**Option 2, plus one immediate demolition.** Keep the CRDT at rest as the kernel's storage and text engine — it is working, it carries fork/replay/journal, and ripping it out is a storage-engine rewrite with no user-visible payoff. But stop exporting the CRDT to clients as the sync contract. The kernel should project a plain, server-sequenced block-event stream that a client can consume without linking diamond-types, and no new client should ever again implement `SyncedDocument`/op-merge machinery. The python player's slice 1 already made this exact bet; formalize it as doctrine rather than a deferral. And delete `BlockDocument` this week — it is 3,016 lines of dead weight with a stale "still used downstream" comment.

The honest headline: **the merge engine is not what's been paying rent, and it's also not what's been costing you.** The oplog, the per-block text buffers, fork, and replay pay rent daily. The *client replication story* is what costs — and the bugs live in the bespoke mirror/projection plumbing around the CRDT, not in DTE merge itself. So the right move is not "delete the CRDT," it's "stop making every client a replica."

---

## (a) Where is merge actually load-bearing?

I traced every write path. The answer is narrower than the architecture implies.

**Verified: the Bevy app is a read-only mirror.** `kaijutsu-app` never calls `push_ops` (grep confirms: only `kaijutsu-mcp/src/doc_task.rs:387` and test harnesses call it client-side). App keystrokes go out as positional `edit_input` RPCs (`kaijutsu-app/src/input/systems.rs:858` etc.) which the *kernel* applies to its input doc. The app applies server ops locally to render (`kaijutsu-client/src/sync.rs:546`), but that is one-way replication — merge as a decoder, not as concurrency resolution.

**Verified: the MCP is sole-writer by construction.** `kaijutsu-mcp/src/doc_task.rs:17` — "one task … the TRUE sole writer. Every mutation arrives as a DocCommand on one mpsc channel." This is the **only genuine distributed CRDT writer in the system**: it authors blocks in a local replica and pushes ops that the server folds in via `merge_ops` (`kaijutsu-server/src/rpc.rs:4317`). And note what it is: 1,044 lines of concurrency control whose entire purpose is to *re-impose single-writer discipline inside a CRDT client*. The multi-writer freedom the CRDT promises was, in practice, dangerous enough that you built a serializer on top of it. That's the single most damning data point in this analysis.

**The vi editor does not client-merge.** `kernel/src/editor.rs`: sessions apply edits kernel-side via `block_store.edit_text` (positional, serialized by the kernel), and concurrent sessions reconcile via `reconcile_block` → `apply_remote_text` — a text-diff against the merged kernel truth, not a DTE frontier merge between principals. Two humans in vi on one block works because the **kernel serializes**; the DTE doc is the canonical buffer, not the referee.

**Human typing + model streaming on the same block:** both funnel through the kernel's BlockStore under its lock. No concurrent frontiers ever form. Same story for the compose input doc — `input_doc.rs`'s comment says "concurrent edits from multiple participants merge automatically," but no participant pushes input ops; everyone sends positional RPCs. That comment describes a capability nobody exercises.

**So the ledger for MERGE (concurrent-frontier resolution) is:** one seam — kernel ⇄ MCP hook mirror — where true concurrency is possible but rare (the CC producer writes; kernel-side drift/mailbox writers to the same cc-context are occasional). Everywhere else, the CRDT functions as a fancy oplog with a nice delta-sync primitive (`ops_since(frontier)`), and even that is shadowed by the per-context `seq_num` gap-detection ladder in `subscriptions.rs` — you already run a server-sequenced event stream as the primary transport; the CRDT ops are just its payload encoding.

The mailbox is your real concurrency-control mechanism, and it's application-level: tool_use/tool_result adjacency, turn atomicity, hydration boundaries. CRDT merge cannot preserve any of those invariants — which is *why* the mailbox and the sole-writer task exist. The kernel already mediates a lot, as Amy said. It mediates the parts that matter.

## (b) Honest cost inventory

- **The pin.** `Cargo.toml:163-165`: TEMP git pin of `diamond-types-extended` for a macOS fix "merged but unreleased on crates.io," dragging a rand-0.8 pin along via jumprope. Small but it's supply-chain surface you personally maintain (it's your fork).
- **Dead legacy model.** `BlockDocument` is 3,016 lines (`kaijutsu-crdt/src/document.rs`). Outside the crdt crate, its only non-comment use is one client *test* (`synced_document.rs:1034`). The `lib.rs` claim "still used by downstream crates during migration" is stale. Every future contributor pays reading tax on it; roughly half of lib.rs's tests exercise it.
- **Client sync burden.** `sync.rs` + `subscriptions.rs` + `synced_document.rs` + `document_store.rs` ≈ 5,600 lines, plus the 1,044-line doc_task. The python-player review — your own document — called the doc task "~1000 lines of hard-won concurrency control — do not port it until the connection layer is proven" and deferred replication as the hairiest part. When the design review for your *next client* treats your sync layer as a hazard to route around, that is the system telling you the client contract is wrong.
- **Known mirror bugs.** The `BlockDeleted` pump bug (issues.md, kaijutsu-acp) is characteristic: the failure is in *bespoke event application to a mirror*, a layer every client reimplements. Three clients (app, MCP, ACP) means three projection implementations and three bug surfaces.
- **Footguns carried.** Principal-major BlockId iteration vs `block_ids_ordered()`; char-vs-byte indexing (fixed, but it happened); clients must link DTE just to decode `BlockTextOps` (`subscriptions.rs:57` — the payload is serialized DTE ops).
- **Two mental models.** Server-authoritative RPC and CRDT replication coexist. Every new surface has to decide which one it is, and the dossier shows the decisions have been drifting toward RPC anyway (app input, vi, python player). Also telling: when model config needed real structure, it left the CRDT for SQL (2026-08-03) without ceremony.

## (c) Steelman — what the CRDT genuinely buys, and whether each needs merge

| Benefit | Real? | Needs MERGE, or just a durable ordered log? |
|---|---|---|
| Deterministic replay/restart (versioned CBOR journal, `journal_op` chokepoint) | Yes, daily | **Ordered log.** Replay applies sequentially. |
| `kj fork` + fork-filters | Yes, core feature | **Log + snapshots.** Filters operate on headers/snapshots, not frontiers. |
| Crash consistency | Yes | Ordered log with the DB-first write-through you already enforce. |
| Live multi-client mirrors | Yes | **Sequenced event stream** — which already exists (seq_num + resync ladder). Merge only decodes the payload. |
| Frontier-based delta sync (`ops_since`) | Elegant | Duplicated by seq-gap refetch. One of the two is redundant. |
| Vi collaborative block editing | Yes | Kernel serialization + a good text buffer. DTE is a *fine* buffer; merge between principals is unused. |
| Config/rc CRDT ownership | Yes — but the win was **sole ownership** (killing host-file dual-truth), not merge. A SQL blob would have delivered the same doctrine. | No. |
| Python player (more first-class writers) | Coming | Slice 1 chose `subscribe_events` + `get_blocks_query` + RPC authoring — explicitly *not* replication. If it succeeds, writers don't need to be replicas. |
| Multi-kernel federation / offline / local-first | Speculative | **This is the only future that truly needs merge.** Kernel-to-kernel peering across moltar/zorak/mac with partitions is the CRDT's home turf. But it's mused, not planned — and the existing cross-context primitive (drift) is application-level transfer, not doc merge. Recorded doctrine is *thin client, smart kernel*, which cuts against option 4. |

## (d) Position, migration sketch, counter-argument, empirical questions

**Recommendation: Option 2 — CRDT at rest, projected stream at the edge.**

Not (1): the status quo makes every client a replica and the python-player review already flinched from it. Not (3): the kernel-internal machinery (BlockStore, per-block DTE buffers, journal, fork) is sound, tested, and load-bearing; deleting it is a rewrite that buys nothing — the costs are at the client seam, not the core. Not (4): it contradicts thin-client doctrine, triples the surface where the mailbox's atomicity invariants can be violated, and solves a problem (offline clients) nobody has asked for.

**Concretely:**

1. **Now:** Delete `BlockDocument` + its tests; fix the stale lib.rs comment. Pure win, ~3k lines.
2. **Slice with the python wheel:** Add a plain-projection variant of the block event stream — `BlockTextDelta {pos, insert, delete}` or small-block full-text, alongside the existing DTE-ops events — so a client can render live text without linking diamond-types. The seq/resync ladder already exists; this is a payload option, not new transport. The wheel becomes the reference thin client.
3. **Then:** Migrate the MCP mirror from local-replica-plus-push_ops to RPC authoring (`insert_block_as` exists) + projected-stream mirroring. If that lands, the doc task shrinks dramatically, `push_ops` has zero production callers, and you can feature-freeze client-side merge without deleting it.
4. **Keep** the DTE engine kernel-side as text buffer + journal encoding; keep the pin until upstream releases, then drop it.
5. **Do not build** client-peer replication for any new surface unless federation becomes a real roadmap item — at which point revisit deliberately, because the at-rest format you kept is exactly what reopens that door.

**Strongest argument against this position:** the bugs cited (BlockDeleted pump, mirror desyncs) live in the *projection* layer, and option 2 keeps projection while sidelining the merge engine — arguably deleting the sound part and keeping the buggy part. Answer: the bugs exist because projection is reimplemented per client; option 2 centralizes it into one kernel-owned contract tested once. But if the kernel-side projection turns out just as fiddly as three client mirrors were, the simplification is smaller than claimed. Second-strongest: if multi-kernel federation is closer than "mused," strangling the client replication path now means rebuilding it later — sunk cost cuts both ways.

**Empirical questions that would most change the answer:**

1. **How often does `merge_ops` see a genuinely concurrent frontier vs. a fast-forward?** Add a counter at `kaijutsu-server/src/rpc.rs:4317`. If real sessions show ~zero true merges, the merge path is production-untested risk, and freezing it is safe. If it fires regularly, the MCP seam is doing real work and step 3 needs more care.
2. **Is federation a 12-month goal?** A yes converts this recommendation from option 2 toward "keep client replication alive and harden it," because that's the one future where merge earns its keep.
3. **Does the wheel's events+queries surface survive contact with a real notebook/agent workload** (Lagged-resync behavior, `get_blocks_query` latency under streaming)? Slice 1 is a live experiment on exactly the option-2 contract; its result is the verdict.

One reframe to close, in the project's own language: the instrument philosophy says many hands, one trust boundary, kernel as the instrument's body. A CRDT-peer client is a second body. Everything the codebase has actually shipped — the mailbox, the sole-writer task, kernel-owned vi sessions, RPC input — has been the system insisting there is one body. The code already voted. Option 2 just writes the vote down.

---

*Verification notes: dossier claims 1–5 all checked against main, with two corrections — (i) lib.rs's "BlockDocument still used by downstream crates" is stale (only comments and one test remain); (ii) the input-doc's "concurrent edits merge automatically" describes an unexercised capability — all input writers use positional RPC.*

---

# Part 2 — The Lean-In Question (same day)

Amy, after reading Part 1: *"what if we actually leaned in to CRDT more — we
have that sftp plumbing, but what if clients could say 'I allow kaijutsu
kernel to edit ~/src/kaish via CRDT', flowing over the ssh connections —
does that change the game?"* Deliberated by gpt-5.6-sol (kaibo batch lane,
max thinking; dossier by gemini after the gpt explorer TPM-starved — see
kaibo's hybrid-cast backlog entry). Verdict summary; full text preserved
below the fold in git history of this commit.

**Verdict: it changes the CRDT argument, but does not justify a general
distributed filesystem.** The idea introduces the first workload with
genuinely independent writers and partitions (laptop offline + kernel both
editing from a shared causal base, merging on reconnect) — exactly where
CRDT merge earns its keep. The revised doctrine:

- **Keep option 2 for conversations and ordinary clients** (projected
  streams; every Part-1 migration step stands, including the BlockDocument
  demolition — merged 75e31b60 the same evening).
- **Amend "never build client replication"** to: *do not make general
  clients replicas; permit replication only for an explicitly
  partition-tolerant WORKSPACE surface, through one shared
  WorkspaceReplica implementation with its own protocol and product
  boundary.* Do NOT reuse conversation SyncedDocument for it.
- **Git is the make-or-break issue**: git stays authority for history and
  baselines; the CRDT is a live uncommitted text overlay keyed to a
  workspace epoch (base commit + generation). Checkout/rebase/stash PAUSE
  or DETACH the replica — never replicate as bulk edits (the
  publish-your-checkout-into-everyone's-tree failure). Dedicated git
  worktrees are the first environment. brak/git carry baselines; CRDT
  carries the overlay; sftp/CAS carry snapshots and binaries. Complements,
  not competitors.
- **Smallest honest probe**: one kernel, one grant, one throwaway git
  worktree, pre-existing UTF-8 source files only, no tree mutations, ops
  over the existing capnp channel, persistent oplogs both sides. Test the
  real partition: disconnect, edit both sides (different lines / adjacent /
  same token), reconnect, verify convergence; crash both sides mid-flow;
  then a deliberate checkout experiment to validate the epoch boundary.
  Go/no-go: (1) no-loss convergence through offline+crashes+editor
  atomic-save patterns, AND (2) repeated workflows where live sync beats
  "agent commits, human cherry-picks". If either fails: STOP before any
  tree layer; sftp becomes explicit staging/import/export + artifact
  hydration instead.
- **First host**: a small Rust `kj-workspace` sidecar reusing
  kaijutsu-client + the existing diamond-types dep; the Python player is
  the launch/UX surface (grant, preview, pause, status), NOT the
  replication engine.
- **Safety floor** (shared-trust ergonomics, but the OS boundary is real):
  exact root+pattern scoping; .git/secrets/build-output excluded by
  default; no symlink following; mass-change circuit breaker;
  live/preview/paused modes; atomic writes + deletion quarantine;
  immediate revoke; per-machine materialization status; audit identifying
  kernel/agent/machine/transaction. CRDT history does not give free undo —
  "restore this content" as a compensating edit is the honest v1.
- **Evidence gaps named**: capnp server→client callback ergonomics, sftp
  upload support, DTE python-binding maturity, the brak protocol, and —
  most important — whether humans and agents actually edit the same dirty
  files concurrently in practice, vs exchanging commits. If real workflows
  are commit-shaped, this is a seductive scope trap and the economics say
  stop.

Fleet-planning note (same evening, zorak session): brak-as-kernel is ONE
kernel with many tailnet clients — it feeds Part 1's one-body verdict and
does not itself require this workspace surface; but if the workspace probe
succeeds, kernel-on-brak materializing agent edits across the fleet is the
natural deployment.

---

# Amy's Ruling (2026-08-09, evening)

*"Code will probably go via git and sftp. So ok yeah, we can release
clients from participating. Is that the figma model?"*

- **Option 2 is DECIDED.** Clients are released from replication: they
  consume the kernel's sequenced projected stream and author via RPC;
  no client links diamond-types. The kernel keeps the CRDT as its private
  at-rest engine (journal, fork, per-block text merge).
- **The workspace probe is SHELVED, not deleted.** Amy answered sol's
  decisive behavioral unknown from the source: her code workflow is
  commit-shaped (git + sftp/brak/launcher). The Part-2 design (epochs,
  grants, pause-on-checkout, WorkspaceReplica boundary) stays here as the
  drawer plan, revived only if observed workflows (e.g. via the sessions
  lens) ever turn genuinely co-edit-shaped.
- **The Figma analogy is apt and adopted as shorthand**: central sequencer
  wins, fractional indexing for order (their child ordering ≈ our
  order_key), clients as optimistic views, CRDT-inspired parts kept where
  cheap, peer merge rejected because a server exists. Kaijutsu differs by
  keeping a true text CRDT + journaled oplog kernel-side (no LWW text
  loss; replay + fork), i.e. Figma's client contract with a stronger
  engine behind it.

Migration steps now unblocked as scheduled work: plain-projection stream
variant (with wheel slice 1), MCP doc-task migration to RPC authoring,
merge_ops concurrency counter (instrumentation), BlockDocument demolition
(already merged, 75e31b60).

---

# Coda — DTE Doctrine (Amy, same evening)

- **No new DTE integration.** No new surface may depend on DTE op
  encoding; the kernel's projected stream is the client contract. CRDT-
  based features are admitted deliberately, on merit ("let the CRDT
  things we want in") — never by default coupling.
- **Refine, don't shed.** DTE's path is refine-in-place rather than
  scheduled removal: Amy owns diamond-types-extended and is effectively
  its only consumer (kaijutsu, plus ~/src/wringer — an older project she
  may revive). Ownership means the fork can be pruned and shaped to
  kaijutsu's exact kernel-internal needs (rope + journal + replay) on our
  own schedule, with wringer kept in mind. The shed-vs-keep question from
  the session dialogue dissolves: vendoring-by-ownership already
  happened; refinement is the lazy gentle ramp.
- Instrumentation still wanted before any refinement pass: merge_ops
  concurrency counter + kernel-internal "did merge do non-trivial work"
  twin.
