# The shell envelope

`kj shell 'echo hi'` and an MCP `shell` call return the same thirteen keys.
One tool name, one shape, one error rule.

```json
{"stdout":"hi\n","stderr":"","exit_code":0,"status":"done","did_spill":false,
 "data":null,"latch":null,"block_id":null,"background_id":null,
 "content_type":null,"ephemeral":null,"elapsed_ms":3,"error":null}
```

The type is `kaijutsu_types::shell_envelope::ShellEnvelope`. It is the single
definition both builders construct.

## Two builders, one shape

The `shell` tool is reached two ways, and each way has its own builder:

| | reaches it | has in hand |
|---|---|---|
| `kaijutsu-kernel` `mcp/servers/shell.rs` | the in-kernel model agent, the RPC seam, rc | a kaish `ExecResult` |
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
there is no code to report: kaish refused the program before running it, or
the code has not replicated to this caller yet.

**`status` carries what one bit cannot.**

| status | meaning |
|---|---|
| `done` | ran, exited 0 |
| `error` | ran, exited nonzero |
| `rejected` | kaish refused the program; nothing ran, fix the text and retry |
| `running` | backgrounded; output streams into `block_id` |
| `timeout` | gave up waiting for the outcome |
| `stream_closed` | the event stream closed before the outcome arrived |

**Truncation is not failure.** A capped result (kaish `did_spill`: exit
remapped to 3, real exit in `original_code`) is judged by the command's real
exit. Flagging it an error tempts a model into re-running a command that
already succeeded. The capping stays visible as `did_spill: true`.

## The body is the envelope

The envelope is what the model reads, not a side channel next to a prose body.
In the kernel it rides `ToolContent::Json`; over MCP it rides both `content`
text and `structuredContent`.

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

The agentic loop splits them (`llm_stream.rs`, step 4a): it recovers the
envelope from the flattened tool body with `ShellEnvelope::from_tool_result`,
takes the output for the block, runs the ANSI projection on **that**, and hands
the model the same cleaned text back inside the envelope
(`with_clean_output`). Both readers end up with one text and one set of byte
offsets, which is what every edit and exclusion range depends on.

Recognition is by deserialization, not by tool name: every field is required,
so a body that is not an envelope cannot be mistaken for one.

The ANSI order matters and is easy to get backwards. An envelope's JSON spells
an escape as the six characters `\u001b`, not a raw `0x1b`, so projecting the
envelope finds nothing and strips nothing — the escapes then reach the model
inside `stdout`. Project the output, then rebuild the envelope.

## Known gap

The MCP path applies no size limit to the envelope
(`CallToolResult::structured`). The kernel path is bounded by the broker's
`max_result_bytes`; a huge stdout over MCP is not. `docs/issues.md` carries it.

## Adding a field

Add it to the struct, to `ShellEnvelope::KEYS`, and to `output_schema()`.
`schema_declares_every_key` and `every_key_is_present_even_when_unset` fail
until all three agree, and `the_envelope_carries_every_shared_key` fails until
the MCP builder fills it or leaves it explicitly `null`.
