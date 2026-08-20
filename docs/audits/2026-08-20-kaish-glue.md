# Kaish glue & integration — kaibo review, 2026-08-20

Reviewer: kaibo `crusoe` cast (synth zai/GLM-5.2, explorer DeepSeek-V4-Flash),
read-only over `crates/kaijutsu-kernel` at `7f3ab694`. The sandbox had no
cargo registry, so U3/U5 below are marked unverified by the reviewer; the
lead verified B1 and B2 by reading the cited code the same day.

## BUGS

None live. The B1 divergence is debt with a silent-fallback smell, not a
confirmed defect.

## 1. Buildup

**B1 (M, lead-verified). Two `apply_context_config` implementations.**
`EmbeddedKaish::apply_context_config` (`runtime/embedded_kaish.rs:600-628`)
runs one `export KEY='…'` kaish script per durable env var with hand
shell-escaping, and `warn!`s a failure away. `KjBuiltin::apply_context_config`
(`runtime/kj_builtin.rs:113-149`) sets the same vars directly with
`scope.set_exported` — no shell round trip, no escaping, no swallowed error.
A third site, `mcp/servers/shell.rs:295-310`, builds a child-process env
`Vec<(String,String)>` for background exec and is a different shape (stays).
Action: collapse the `EmbeddedKaish` path onto the scope API; a failure to
seed env must bubble, not warn.

**B2 (M, lead-verified). `KaijutsuBackend`'s file half is a dead editing
surface.** `KaishKernel::with_backend` receives `MountBackend`
(`embedded_kaish.rs:425`); `KaijutsuBackend` (`runtime/kaish_backend.rs`) is
reachable only through the `/v/docs` adapter (`runtime/docs_filesystem.rs`),
whose `write` always uses `WriteMode::Overwrite` (`:82`) and exposes no
patch/append/range. So `write`/`append`/`patch`/`mkdir`/`remove`/`rename`
plus `compute_patch_op`/`check_byte_boundary`/`apply_read_range`
(`kaish_backend.rs:782-1030`, ~250 lines) are unreachable through the mount.
The tool-dispatch half (`convert_tool_info`, `tool_args_to_json`,
`call_tool`, `:148-220`, `:677-723`) is live and stays. Action: audit live
callers of the file half; make the unreachable ops return
`InvalidOperation` like the symlink stubs already do, and delete the helpers.

**B3 (S). Stale kaish 0.9–0.14 comments.** Pure archaeology: the 0.9
`mcp()`→`agent()` rename (`embedded_kaish.rs:349`), the orphaned latch note
(`:357`), the 0.9 Blob→Bytes note (`kaish_backend.rs:1034`).
`background_exec.rs:22-34` cites `kaish-kernel-0.13.0` file:lines that are
unverifiable against the pinned 0.15. Keep the ones that explain a current
decision: the bare `--confirm` flag (`kj_builtin.rs:596`), latch-on-baggage
(`:852-862`), background-exec's JobManager non-use.

**B4 (S now; deletable on a kaish bump). `OutputProfile::Internal` + the
4 MiB `INTERNAL_OUTPUT_LIMIT_BYTES`** (`embedded_kaish.rs:90-144`) exist
because kaish's `did_spill` exit-code remap forges a failure in scripts that
consume their own output. A kaish knob that signals spill without remapping
the exit code collapses the enum, the constant and `to_config`.

**B5 (S; resolves with B2).** `apply_range` (`mount_backend.rs:169-195`) and
`apply_read_range` (`kaish_backend.rs:782-801`) both window a
`kaish_kernel::ReadRange`.

## 2. Under-use

**U1 (M). The `kj` builtin flattens kaish's typed `ToolArgs` back to argv.**
`runtime/kj_builtin.rs:487-594`: ~110 lines rebuilding `Vec<String>` from
`positional`/`named`/`flags`, with special cases for repeatable flags (the
`--include` bug), skipping `--json` (kaish owns it), skipping then
re-extracting `--confirm`, and `wants_stdin_content`. kaish already parsed the
statement against the clap schema we reflect with `schema_tree_from_clap`;
`KjDispatcher::dispatch` then re-parses with clap. Action: let `dispatch`
accept `ToolArgs` (or `ArgMatches`) and delete the flattening block and its
special cases. Removes the "kaish parsed it one way, clap another" bug class.

**U2 (L; blocked upstream, not actionable).** `background_exec.rs` (~900
lines) reproduces kaish's job manager because a throwaway kaish per call has a
throwaway `JobManager`, and `execute_background` does not forward live bytes.
Both reasons still hold on 0.15. Collapses to "one long-lived kaish, use `&`"
if kaish ships a reusable, streaming job manager.

**U3 (S; forward-looking).** The MCP `edit` tool (`file_tools/`) is a bespoke
hashline engine that does not use the `PatchOp` path `MountBackend::patch`
implements for the shell. When kaish 0.15.1's `edit` lands, retire-vs-trim is
already the open question in `docs/file-buffers.md` slice 4.

**U4 (S; low — robustness beats brevity).** `load_rc_scripts`
(`kj/lifecycle.rs:368-376`) re-implements `ls | sort` in Rust; the Rust
loader's fatal-on-read-failure and `RunGuard` run-log are harder to express
in-script. Recorded so nobody proposes the script version without that cost.

## 3. Verified not debt (do not re-litigate)

- `command_substitution_survives_a_non_last_pipeline_stage`
  (`embedded_kaish.rs:776-804`) — canary with instructions; pins kaish
  #367/#368.
- `kaish_ls_and_cat_reach_the_real_cas_mount_at_v_cas` — canary for the 0.12
  `/v/cas` shadowing fix.
- `OutputProfile::Internal` — deliberate divergence until the upstream knob.
- `background_exec.rs` not using `JobManager` — documented structural reasons.
- `KaijutsuBackend` symlink refusal — rc composition routes through
  `ConfigDocFs` on purpose.
- `--json` handling in `KjBuiltin::execute` — correct adoption of kaish 0.13
  output ownership; `kj` does not build its own envelope.
- Latch-on-baggage (`kj_builtin.rs:850-923`) — the correct post-0.14 shape.
- `ReadOnlyFs` — the `/v/*` mounts bypass `MountBackend::new_read_only`, so
  the wrapper is required.
- `merge_trace_context` — W3C trace injection kaish does not provide.
