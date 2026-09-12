# Approval identity

The character performing an operation cannot approve it. Its assigned reviewer
can, from any context. A reviewer may be Amy or a lead model directing coders.

```sh
kj character create coder
kj context create repair --type coder --as coder
kj context info repair
```

The caller's character becomes the new context's reviewer. When Amy creates
`repair`, Amy reviews coder. When Banto creates it during a model turn, Banto
reviews coder even if Amy requested Banto's turn.

To assign an existing context, its current reviewer runs:

```sh
kj context set repair --as coder --reviewer banto
```

When a context has no reviewer yet, its creator may make the first assignment.
Both names must resolve to live characters, and the performer and reviewer
must differ. Ordinary forks preserve both assignments. A model turn with an
unset, missing, retired, or self-reviewing assignment fails with instructions
for correcting it. Regular client creation leaves the performer unset.

## Three identities

| Field | Meaning |
|---|---|
| `principal_id` | Authenticated requester; retained for capability and redemption scope |
| `actor_id` | Character performing this invocation; the context's `played_by` for kernel model work |
| `reviewer_id` | Character assigned to evaluate the performer's asks |

The kernel resolves a model turn's identity once before it starts. Tools,
read-only and writable shells, nested `kj` commands, and hook bodies preserve
that invocation identity. Provider text, reasoning, and tool calls are authored
by the performer. User prompts keep their requester; kernel-produced tool
results keep kernel authorship.

An app, TUI, ACP client, or MCP bridge acts as the character bound to its
credential. Navigating into a coder context does not turn Amy into coder.
External lead models need their own character-bound credential; two processes
using the same credential are the same character. The `user_initiated` flag
controls presentation, never approval authority.

New ACP sessions select a kernel model performer at launch:

```sh
kaijutsu-acp --character coder
```

The connected character becomes its reviewer. Without `--character`, use
ACP session loading to attach to an already configured context. The flag
selects a character; `--context-type` still selects the rc bundle.

## Answering and withdrawing asks

```sh
kj ledger show <request-id>
kj ledger allow <request-id>
kj ledger deny <request-id>
kj ledger escalate <request-id> --to amy
kj ledger cancel <request-id>
```

An ask snapshots its requester, performer, and reviewer. `show` reports all
three plus the deciding character. The assigned reviewer alone may allow,
deny, or escalate a pending ask. Escalation records both reviewer identities
and refuses to assign the performer. The requester or performer may cancel a
pending ask. Cancellation runs nothing; it does not undo an executed action.

The TUI displays the authenticated character and offers approval controls only
to the assigned reviewer. A second context is unnecessary. The global ledger
also exposes asks raised in other contexts. ACP permission prompts follow the
same eligibility rule; the kernel remains authoritative if the assignment
changes while a prompt is open. Timeouts, cancelled ACP prompts, and transport
errors leave the ask pending; only an explicit decision records a verdict.

A directing model reads the coder's response and `kj ledger list|show` to
evaluate its asks. Reviewer assignment does not automatically schedule a
turn in one of that character's contexts.

Direct commands retain their connected actor. If that actor has no distinct
reviewer, it cannot confirm its own ask: it can cancel it or escalate it to a
different character. Human status is not inferred from a client type.

## Persistence and replay

Redemption matches the requester, performer, context, statement, and label.
Changing performers revokes that context's learned session rules. Broader
rules remain governed by their explicit scope. A linked model approval whose
performer changed is consumed without execution; restoring the old performer
does not revive it. Malformed or partial output-pair linkage refuses replay.

Old asks with no recorded performer/reviewer remain audit history and cannot
be approved through a guessed identity. Existing contexts need explicit
assignment before their next kernel model turn. This change supplies no
context-based approval compatibility path.
