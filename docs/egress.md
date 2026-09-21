# Egress

Which hosts a context's shell may reach over the network, who decides, and what
a refused request looks like. Today this covers `curl`, the only network tool a
context shell registers. `git` and any later builtin that contacts an outside
resource go through the same policy, so one place monitors, classifies, and
constrains them. See `docs/issues.md`, "Egress: what stays open".

## The rule

A context reaches a host only when that context's egress list names it. A
context with an empty list reaches nothing: `curl` stays registered, refuses
the request before any connection opens, and its error names the host and says
the context's egress list does not allow it.

The list belongs to the context, not to its type and not to a file. It is rows
in `kernel.db`:

```
context_egress(context_id BLOB, host TEXT, PRIMARY KEY (context_id, host))
```

`host` takes one of three forms:

| Form | Meaning |
|---|---|
| a DNS name, `crates.io` | that exact name; no subdomain matching |
| a loopback literal, `localhost`, `127.0.0.1`, `::1` | that loopback address |
| `*` | every host, loopback included |

A DNS name that resolves to a loopback or link-local address is refused unless
the list also holds a loopback literal (or `*`). `kaish-tools-curl` checks each
resolved address before it connects, so a name cannot stand in for an address
the list did not grant.

Loopback is not granted by default. On a host like zorak it reaches the
kernel's own SSH port and local model servers, so a context gets it only when
its list says so. Tests that stand up a local mock server add `127.0.0.1` to
the test context's list.

## The classifier host

The lfm2d pre-call hook runs in a snapshot of the calling context's shell and
calls the classifier with `curl`. A context with an empty list must still be
gated, so `runtime/curl_tool.rs` keeps one built-in host, the classifier's,
that every context reaches. It is the only host the code names. Moving it into
the rows needs a way for rc to grant it at context creation without giving the
context authority over its own list; see `docs/issues.md`.

## Who changes the list

`kj context set <context> --egress-allow <host>` adds a row and
`--egress-deny <host>` removes one. Both repeat. The caller must hold the same
authority a performer assignment needs: the target's lineage root, its
effective reviewer, or its director (`kj/context.rs`, `context_set` and
`caller_may_assign_performer`). A model-played context is none of those for
itself, so it cannot widen its own list. `kj context info` shows the list.

`kj fork` copies the parent's rows to the child, so a child starts with what
its parent had and no more. `kj context create` starts with an empty list.

The shell reads the rows when it is built, and a context shell is built for
each use, so a change applies to the next command. A running `curl` keeps the
list it started with.

## What a miss does

A host that is not on the list is refused. Nothing asks a human and nothing
scores the request. The choices today are the list or `*`. Sending a miss to a
classifier or to the approval ledger is open work: the gate's plan evaluator
already sees `curl <url>` as a command, but an approval has no way to reach the
tool at connect time.
