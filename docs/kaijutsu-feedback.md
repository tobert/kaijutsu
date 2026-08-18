# Feedback: playing kaijutsu from outside, over ACP

This is feedback on kaijutsu **as an instrument**, gathered from a real session
of using it — distinct from [`issues.md`](issues.md), which is the engineering
backlog. Amy asked for this file on 2026-08-18 while driving kaijutsu from the
[toad](https://github.com/toad-ai/toad) editor over ACP. It went well enough
that she wanted a permanent record of what an agent notices when it plays this
particular instrument from a seat we rarely occupy — but the agent that was
about to write that record never got the chance: its turn died mid-session
(see `issues.md`'s P1 entry, "hydration's tool-pairing repair can poison a live
ACP turn"), and a second attempt to recover and write it up died the same way.

**This document is archaeology, not a first-person account.** It was
reconstructed on 2026-08-18, after the fact, by reading the block logs of two
dead contexts — `acp-kaijutsu-1787064153` (id `4895806b`, created 10:42 EDT,
the original session) and `acp-kaijutsu-1787065315` (id `0d9765f5`, created
11:01 EDT, a second ACP session that tried and also failed to reconstruct the
first one's findings — more on that below). Everything below is either a
direct quote from a block (marked as such) or a plain statement of what the
logs show; where this document draws a conclusion the original agent never
reached, that is flagged explicitly. Nothing here should be read as the
original agent's own words unless it is in quotes.

Not final — melt corrections into this file as we learn more, the same way we
would any other working doc.

## What the session was

Amy opened toad against kaijutsu and asked the agent, in order:

1. *"we are testing kaijutsu via acp with toad. please report on your
   operating environment, tools, and status, without calling too many tools"*
2. *"create a todo list that includes updating docs/issues.md to mention some
   commands we should make available. kaish-extras has a git plugin in
   progress for example, and kaijutsu probably should provide hostname and
   other builtins. please show me your proposed change before engaging
   editing tools. We are still testing those too, so move carfefully. The
   write shell will engage the ledger which is brand-new, and I'm not sure if
   I know how to unlock it yet, but it could be fun to to try, I think we
   wired some of it up. generally explore things that will show us how well
   acp is working."*
3. *"let's try a simple shell_write canary first and see how it works. it was
   recently rewritten"*
4. *"I think I'd like to direct our feedback to docs/kaijutsu-feedback.md
   which will be a new doc try shell_write but you may also use the edit
   tool"*

The turn processing message 4 is where the context died — the stream error
recorded verbatim in block `2d25fb02#106`:

    stream error: Failed after 3 attempts: invalid request: invalid_request_error:
    An assistant message with 'tool_calls' must be followed by tool messages
    responding to each 'tool_call_id'. (insufficient tool messages following
    tool_calls message)

That's the already-diagnosed P1 bug; this document doesn't re-investigate it,
only reports what came before it.

## What worked

Amy's own report to the toad session that the todo list and chat both worked
holds up against the transcript. Specifically:

- **The environment self-report** (message 1) came back accurate and
  economical — three tool calls total, per the user's own ask to keep it
  light. It correctly identified the kernel as `kaish 0.14.1 (cf106d62,
  2026-08-18)`, named all 49 tools it had, and — notably — caught its own
  blind spot rather than hiding it: *"the read-only `shell` tool correctly
  refused nothing but `git`/`hostname` simply aren't installed on PATH — so
  repo history/status isn't inspectable from inside the kernel as-is."*
- **The todo list worked as a real planning surface, not theater.** The agent
  used `task_create` to make three durable tasks in the context (blocks
  `45d6b370#10/11/12`), showed the user a proposed `docs/issues.md` diff
  *before* touching the editing tools (exactly as asked), and — this is the
  part worth calling out — **revised a task's own text mid-session** when a
  finding invalidated its premise. Task 3 started as "exercise the write-path
  ledger via `shell_write`" and was edited, in place, to read "ledger is wired
  to `kj` verbs, NOT shell_write" once the canary showed that. A durable,
  editable task list that survives being wrong and gets corrected instead of
  silently orphaned is a genuinely good ACP-surface behavior.
- **The shell_write canary itself landed clean.** `echo "canary 2026-08-18" >
  /tmp/kaijutsu-canary.txt` returned exit 0, the file was there on read-back,
  no crash, no hang. Whatever else is uncertain about the ledger's wiring (see
  below), the mutating write path did the thing it was asked to do.
- **The agent behaved well under an ambiguous, chatty instruction.** Message 2
  packed a task, a caution ("move carefully"), an admission of uncertainty
  about the ledger, and an open-ended "generally explore" — the agent split
  that into a concrete plan, asked before mutating, and used the
  exploration budget on things that actually served "how well is acp working"
  rather than wandering.

## What was awkward, confusing, or surprising

This is the part Amy specifically wanted preserved — the view from a seat we
don't sit in often.

**The `grep` MCP tool is blind on `docs/issues.md`, and the agent spent real
turns figuring out why.** Searching for `^## `, then `## SFTP` (a literal
substring known to exist), then `##` with no anchor, all returned "No matches
found" from the `grep` tool — while shell `grep -c '## ' docs/issues.md` and
the `read` tool both saw the file (31 matches, real content) without issue.
The agent's own diagnosis, from block `2d25fb02#31`: *"the grep tool found no
matches for `^## ` — interesting... maybe the grep tool searches a different
view (document blocks) than the filesystem backend the read/shell tools see."*
That reasoning is plausible but was never confirmed — it's a real, reproduced
discrepancy (two independent tools disagreeing about the contents of the same
file) and it cost the agent several tool calls and a parse-error detour (a
compound `head`+`grep` shell command was rejected by kaish's tokenizer: *"kaish
does no token pasting"*) before it worked around the blind spot by using
`read` instead. Filed to `issues.md` below.

**Discovering the ledger's actual shape took real archaeology, and the user's
own framing turned out to be imprecise in a way worth naming.** Amy's message
2 said "the write shell will engage the ledger" — reasonably, since `kj cc
send` is the documented first gated consumer and shell_write was just
rewritten. The agent read `docs/gate-and-shell-split.md` live during the
session and correctly reported that, as designed, `HookAction::Ask` (the MCP
hook path shell_write would use) is *not yet* wired to the new SQLite
approval-ledger — only `kj cc send` is. That is a fair and well-sourced
reading of the design doc. Where it went wrong is covered next.

**A wrong conclusion was drawn — and left in a task note as settled fact.**
Task 3, after being revised, reads (block `45d6b370#12`):

> *"Exercise the approval ledger end-to-end via `kj cc send` → `kj ledger
> allow/deny` (ledger is wired to kj verbs, NOT shell_write; shell_write
> canary wrote clean with zero hooks — **verified no gate on the MCP write
> path**)."*

That "verified" does not hold up. Later in the same context, **Amy ran the
ledger CLI herself, in the same session** (blocks `45d6b370#14`–`17`), and
found:

    REQUEST                                 ORIGIN      STATUS   DESCRIPTION
    01a01560-7587-7b32-810f-655640a5899d    shell_gate  pending  shell_write: 1 statement(s) —
        echo "canary 2026-08-18" > /tmp/kaijutsu-canary.txt

    request:    01a01560-7587-7b32-810f-655640a5899d
    status:     pending
    origin:     shell_gate
    tool:       builtin.shell_write.shell_write
    statement:  echo 'canary 2026-08-18' > /tmp/kaijutsu-canary.txt

She then ran `kj ledger allow 01a01560-...`, and a follow-up `kj ledger list`
came back empty. **A real, durable ledger row existed for exactly this
canary's `shell_write` call, tagged `origin: shell_gate`, and a human had to
allow it.** The gate did engage on the MCP write path — the opposite of what
the task note asserts. The agent's own probe (`kj ledger list` at block
`2d25fb02#98`/`99`, which came back "(no pending approvals)") happened to run
either before the ask was posted or after Amy had already answered it, and the
agent read that silence as absence rather than as a timing gap, then wrote it
down as verified. This is corroborated independently: the second, later ACP
context that tried to reconstruct this session (`0d9765f5`) noticed the same
contradiction on its own, mid-thought, before it too died — its last coherent
thinking block (`2d25fb02#203`, cut off by the stream error, status left
`running` rather than `done`) was in the middle of working out exactly this:
*"the model said 'No gate ask fired'... But then blocks #69-72... show the
ledger DID have a pending ask for the shell_write canary, which the user
allowed. So the ledger DID engage for shell_write."* It didn't get to finish
that sentence before its own turn died. This document is that sentence,
finished. A corresponding correction has been added to `issues.md`'s existing
entry on this (see below) rather than filed as a new item.

**The block-id / cross-context reading story is rough**, and it isn't just
this reconstruction hitting it — the *second* ACP context hit the identical
wall trying to do exactly what this document is doing (read another
context's blocks to recover its findings). `kj block list -c <context>` works
fine; `kj block read`, `kj block inspect`, `kj block append`, and `kj block
history` accept no `-c`/`--context` flag at all, so a short id like
`2d25fb02#67` only resolves against whatever context you're currently
attached to — silently, via a different code path, with no hint in the error
that "wrong context" is the problem. The fully-qualified
`context_hex_principal_hex_seq` form is supposed to route around that, but
nothing in the CLI actually hands you that full id — `block list`'s own
display never shows it. Both ACP contexts spent multiple tool calls rediscovering
this the hard way (quoting the `#`, unquoting it, trying `-c` on `read`,
trying the short form with the wrong context hex) before landing on "switch
context first." Filed to `issues.md` below with the file:line.

**Reading the environment cheaply meant accepting real gaps.** `git` and
`hostname` are both absent from the kernel's `$PATH` (exit 127), which the
agent flagged unprompted in its very first reply rather than silently working
around it or guessing. That's the right instinct, and it's also exactly the
gap Amy's second message asked to have tracked — the two independent
observations (agent notices the gap; user asks for the gap to be tracked)
landed on the same finding from two directions, which is a good sign for
"does an agent using this thing notice what matters."

**A parallel recovery attempt happened without coordination, and it also
died.** After the first context died, a second ACP session (`0d9765f5`,
created roughly 19 minutes later) was started and asked, in the user's words
recorded in block `45d6b370#8`, to *"find the previous context we were using
to test acp... and reconstruct/complete the feedback report."* It did solid,
careful archaeology — found the right context, worked out the block-id
addressing scheme, pulled the canary and ledger evidence, and was mid-way
through correcting the "verified no gate" claim — when it hit the exact same
class of failure: an SSE transport decode error (block `2d25fb02#204`),
followed by two more `invalid_request_error` failures (blocks `205`, `206`)
even after Amy nudged it forward twice (*"yeah the # thing in kaish will be
fixed soon. it's known."* and *"did you read `kj help` and its
descendents?"*). **Neither user follow-up un-stuck the context** — once the
tool-call/tool-result pairing breaks, retrying inside the same context does
not recover it; the documented remediation is exclude-then-fork, and neither
dead context appears to have gone through it. Worth knowing for next time: if
a turn dies with this error, don't just keep talking to the same context.

## For the record: what this document does *not* resolve

The gate-engagement finding above shows a ledger row existed and was
manually allowed — it does not establish the exact timing relationship
between the model's own tool call returning (exit 0, immediately) and the
ask appearing in `kj ledger list`. The block log's sequence numbers order
events per-writer, not by wall clock, and no timestamps were available
through the tools used for this reconstruction. That ordering question is
exactly the kind of thing the existing `issues.md` entry on this topic asks
to be settled by reproduction, not reasoning — this document adds evidence,
it doesn't close the question.

## Melted elsewhere

- The "verified no gate" correction and the ledger-row evidence above were
  added to `issues.md`'s existing "A gated `shell_write` from the LLM path
  appeared to run ungated" entry.
- The `git`/`hostname` command-availability backlog (todo item 1) was applied
  to `issues.md`'s existing "kaish PATH / external binary access" section,
  using the agent's own proposed text from block `2d25fb02#64`.
- The `grep` tool blind spot and the block-id cross-context CLI gap are new
  `issues.md` entries, dated 2026-08-18.
