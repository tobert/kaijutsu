# The shell envelope

`kj shell 'echo hi'` and an MCP `shell` call return the same fourteen keys.
One tool name, one shape, one error rule.

```json
{"stdout":"hi\n","stderr":"","exit_code":0,"status":"done","did_spill":false,
 "data":null,"latch":null,"block_id":null,"operation_id":null,"ask_id":null,
 "content_type":"text/plain","ephemeral":false,"elapsed_ms":3,"error":null}
```

The type is `kaijutsu_types::shell_envelope::ShellEnvelope`. It is the single
definition the runtime and transport projections construct.

## Two builders, one shape

The `shell` tool is reached two ways, and each way has its own builder:

| | reaches it | has in hand |
|---|---|---|
| `kaijutsu-kernel` `runtime/command_result.rs` | the in-kernel model agent, the RPC seam, rc | a kaish `ExecResult` |
| `kaijutsu-mcp` `lib.rs` | an external stdio MCP client (Claude Code) | a polled `BlockSnapshot` |

They ran the same command under the same tool name and used to return
different fields with different error semantics. A caller could not parse one
shape without first knowing which path served it.

## Rules

**Every key is always present.** A path that cannot know a value writes
`null`; it never omits the key. `null` means "this path cannot know", which is
never the same as `0`, `false`, or `""`. `did_spill` and `latch` ride kaish's
`ExecResult`, so the MCP path leaves them `null` — the block snapshot it polls
does not carry them.

**A nonzero exit is an error.** Both builders set the tool-result error flag
from `status`, so the flag and the field can never disagree. (Amy, 2026-09-04.)

**`exit_code` null is unknown, never success.** It is `null` exactly when
there is no code to report: kaish refused the program before running it, a hook
supplied a synthetic replacement, or the code is unavailable to this caller.
A replacement's `status` describes the hook result; its raw command exit remains
in the runtime execution record.

**`status` carries what one bit cannot.**

| status | meaning |
|---|---|
| `done` | exited 0, or a successful hook replacement |
| `error` | nonzero exit, execution/persistence failure, or an error hook result |
| `rejected` | kaish refused the program; nothing ran, fix the text and retry |
| `running` | accepted asynchronous operation; `operation_id` identifies its receipt |
| `waiting` | accepted operation is waiting on `ask_id`; this is not an error |
| `timeout` | gave up waiting for the outcome |
| `stream_closed` | the event stream closed before the outcome arrived |

**Truncation is not failure.** A capped result (kaish `did_spill`: exit
remapped to 3, real exit in `original_code`) is judged by the command's real
exit. Flagging it an error tempts a model into re-running a command that
already succeeded. The capping stays visible as `did_spill: true`.

## Runtime outcomes

Interactive commands and approval resumes use `CommandOutcome`. The raw kaish
result and any hook replacement or refusal remain distinct. Terminal outcomes
are retained before projection. Receipts commit against that record before
blocks advertise completion. Startup finishes pending projections without
executing the command or hooks again, and preserves edits made after terminal
publication. A hook replacement clears the old stderr, physical exit, content
type, ephemeral flag, and structured output. Its text/JSON content and structured
payload supply the new result. Raw records are separate from ordinary receipt
polls. Structured RPC, streaming RPC, and MCP completion are still being migrated;
see `docs/kaish-integration.md`.

Owner cancellation (kernel shutdown or a caller's disconnect) can end result
review while a hook is still paused or running on already-captured output. The
captured execution is retained exactly as run; only its publication stops.
This is not a hook refusal and not the command's own exit: the job and RPC
adapters that need an integer report the kernel's existing cancellation code,
130, so a script can tell interruption apart from both. The envelope still
claims no physical exit for it, the same as any other hook effect.

## The body is the envelope

The envelope is what a tool returns, not a side channel next to a prose body.
In the kernel it rides `ToolContent::Json`; over MCP it rides both `content`
text and `structuredContent`. A kernel model turn renders it for the model;
see "What a model turn reads".

This is what closed the shape flip a worknote reported. The kernel body used
to be prose — stdout, with stderr and `[exit N]` appended — and the envelope a
separate structured payload. `Kernel::call_tool` substitutes the
pretty-printed structured payload when the text body comes out empty, so a
command with no output returned JSON while every other command returned text.
`cat /dev/null` and `grep` with no matches read as two different shapes.

One consequence is load-bearing: **an oversize JSON body is shrunk inside its
strings, not cut in half.** `truncate_result_to_budget` cuts a text body
head+tail with an elision marker, which on a serialized envelope would leave
the model unparseable text — losing the exit code and status along with the
output that overflowed. `shrink_json_to_budget` elides the middle of the
longest string value instead and repeats, so every key survives and only the
output that did not fit is taken.

## The block is not the envelope

The envelope is what the **model** reads. The durable block holds the command's
output — `readable_output()`, ANSI-stripped — because a block is read by people
in the tui and the app, and replayed by hydration. A JSON object serves
neither.

The agentic loop splits them (`llm_stream.rs`, `dispatch_recorded_tool_result`):
it recovers the envelope from the flattened tool body with
`ShellEnvelope::from_tool_result`, takes the output for the block, runs the
ANSI projection on **that**, and renders the model's text from the same
cleaned output (`model_text`). Both readers share one text and one set of
byte offsets, which is what every edit and exclusion range depends on. The
block keeps the envelope with its output blank as `shell_envelope`, the
record behind what the model read.

Recognition is by deserialization, not by tool name: every field is required,
so a body that is not an envelope cannot be mistaken for one.

The ANSI order matters and is easy to get backwards. An envelope's JSON spells
an escape as the six characters `\u001b`, not a raw `0x1b`, so projecting the
envelope finds nothing and strips nothing — the escapes then reach the model
inside `stdout`. Project the output, then rebuild the envelope.

## What a model turn reads

A kernel model turn sends `ShellEnvelope::model_text`, not the JSON. It is
the readable output, then one bracketed line for each fact that changes what
the model does next:

| Result | Lines after the output |
|---|---|
| exit 0, nothing else | none; an empty result reads `(no output)` |
| nonzero exit | `[exit N]` (`[failed; no exit code]` when none exists) |
| refused before running | `[rejected: the program did not run]` |
| background call | `[running in the background: operation ID]` |
| waiting for approval | `[waiting for approval; not run yet: operation ID, ask ID]` |
| gave up waiting, stream closed | `[timed out waiting; ...]`, `[the outcome never arrived ...]` |
| capped output | `[output truncated]` |
| a `kj` payload or latch | `[data] JSON`, `[latch] JSON` |

`block_id`, `content_type`, `ephemeral`, and `elapsed_ms` are never sent. The
rendering is always text, including for a command that printed nothing: the
earlier prose body returned JSON only when output was empty, which is the
shape flip described above. The turn stores the rendered text as the result's
`model_content` only when it differs from `content`, and hydration replays it
(`docs/conversation-session.md`, "Tool results replay as sent"). The MCP
`shell` tool still returns the full envelope as `structuredContent`.

## Known gap

The MCP path applies no size limit to the envelope
(`CallToolResult::structured`). The kernel path is bounded by the broker's
`max_result_bytes`; a huge stdout over MCP is not. `docs/issues.md` carries it.

## Adding a field

Add it to the struct, to `ShellEnvelope::KEYS`, and to `output_schema()`.
`schema_declares_every_key` and `every_key_is_present_even_when_unset` fail
until all three agree, and `the_envelope_carries_every_shared_key` fails until
the MCP builder fills it or leaves it explicitly `null`.
