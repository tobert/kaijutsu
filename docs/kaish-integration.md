# Kaish integration and rc lifecycle

Kaijutsu owns contextual execution policy and durable outcomes. Kaish owns the language, interpreter, subprocess execution, and in-memory job machinery. Rc owns lifecycle orchestration and uses the same kaish integration as other consumers.

## Ownership

Keep the integration in `kaijutsu-kernel`. Names below describe responsibilities:

| Owner | Responsibility |
|---|---|
| Kaish integration (`runtime`) | Contextual construction, VFS adapters, builtin registration, identity propagation, execution/output policy, environment seeding, tracing, timeout/cancellation, and kaish job access |
| Rc lifecycle | Discover and snapshot executable bodies, order scripts, supply lifecycle facts, enforce recursion limits, record runs, and report lifecycle failures |
| Command execution | Apply command hooks and approval state, run or resume a command, settle its outcome once, and project it into blocks, receipts, and job results |
| Broker hooks | Select hooks and interpret their protocol; use shared execution for kaish bodies without granting rc authority |
| Editor consumer | Execute in opener's context and consume complete stdout for `:r !cmd`; report failure without splicing preview |
| Transport adapters | Decode requests, identify connected player, deliver responses/events, and apply connection-local context switches |
| Kaish | Parse and evaluate programs, perform backend operations and permitted host execution, and manage execution jobs |

Contexts are retained. `kj context archive <id> --confirm` archives a context while preserving lineage, blocks, execution receipts, and approval history.

## Current implementation

- `runtime/admission.rs::ContextAdmission` records accepted context requests before preparation.
- `runtime/embedded_kaish.rs::EmbeddedKaish` wraps the kaish kernel, wiring mounts, file cache, per-context JobManager, output limits, host execution policy, wrapped python, HOME/PATH, and trace propagation.
- `runtime/context_shell.rs` implements `EmbeddedKaish::for_context` with explicit `ShellIdentity` and `ShellPolicy`.
- Cwd persistence and validation ensure absolute paths, robust ancestor fallback, and transactional exports.
- `runtime/editor_read.rs` owns `:r !cmd` on the kernel worker.
- `ShellPolicy::ReadOnly` shells register a builtin read-only `git` tool (`runtime/git_tool.rs`) and use `ExternalExec::LookupOnly` for PATH resolution.
- `rc::run` accepts `RcInvocation` and owns loading, ordering, variables, recursion, run records, and `.kai` execution.
- `runtime/command.rs` and `runtime/structured.rs` own command execution, structured `kj`, and approval resume.
- `runtime/command_outcome.rs` retains raw execution, hook replacement/refusal, elapsed time, and shell-state write failures.
- `runtime/worker.rs::RuntimeWorker` supplies a lazy kernel-owned LocalSet on a reserved kaish stack.
- Model streaming and identity resolution (`runtime/llm_stream.rs`, `runtime/turn_identity.rs`) manage conversation locks, mailboxes, and per-turn leases.
- `runtime/approval_resume.rs` owns answer delivery, claims, captured cwd/env, approved execution, and follow-up seeds.
- `runtime/result_review.rs` checkpoints executed outcomes and links them in PostCall or OnError ask transactions.

## Wrapped python

`python3` and `python` run a kaish wrapped command when shell policy is `ExternalExec::Allow`.
- Registered only when resolving on shell PATH at construction (missing yields exit 127, `command not found`).
- Declares standard Python options (`-c`, `-m`, etc.) with `Stdin::Pipe` and `Tail::Forward`.
- Passes exported variables (`PATH`, `VIRTUAL_ENV`, `PYTHONPATH`) and real cwd.
- `KJ_TOOL_PLAN` exposes `interpreter` objects with source classification (`inline`, `script`, `module`, `stdin`, `none`, `unknown`) and arguments.

## Shared execution contract

Command and tool executions share unified outcome retention, atomic block/receipt generation, hook evaluation, cancellation propagation, and result review semantics. Read-only and writable shells enforce strict policy boundaries around host execution and file mutation.

## Rc Markdown: explicit instruction authoring

Rc lifecycle scripts and configuration use explicit Markdown instruction authoring under `/config/rc/`. Scripts execute within controlled `RcInvocation` bounds with strict recursion limits and durable run records.
