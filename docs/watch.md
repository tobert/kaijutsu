# Watches

```sh
kj watch add AGENTS.md
kj watch list
kj watch show 3
kj watch rm 3
```

A watch tells a context that a file changed. It never starts a turn. Changes
accrue while the context is idle and go with the next prompt, in one kernel
notice placed just before it:

```text
Watched files changed since the last prompt:
- /work/repo/AGENTS.md: changed 2 times, last 4m ago
- /work/repo/notes.md: changed once, last just now; it no longer exists
```

Amy, 2026-11-08: "notifications would accrue and coalesce on idle and be
delivered along with the next user prompt. They should not kick off a
session. Maybe we'll add a way to do that later, but only after we've gotten
some experience with it."

## What a watch holds

A watch is a row in `context_watches` (`kernel_db/watches.rs`): the VFS path
as resolved when the watch was added, the host file behind it, and a status
kept current as events arrive. Only a running watch on a live context
accrues or delivers.

- **Only host files.** `kj watch add` takes an absolute path or one relative
  to the context's working directory. It must name an existing file on a host
  directory mount (`real_path` resolves it). A path on a memory, CAS, or share
  mount is refused.
- **One watch per path per context.** A second `add` names the watch that
  already holds the path.
- **The status is a coalesced count.** Events on one file less than 500 ms
  apart (`path_watch::BURST_MS`) are one change, so an editor's save counts
  once. The status keeps the number of changes, the first and last change
  times, and whether the file existed at the last event.
- **The notice holds no file content.** It names the path, the change count,
  the last change, and whether the file is gone. The model reads the file
  when it wants the content.

## Delivery

A prompt carries the notice: a chat submit, typed or from a draft
(`runtime/prompt.rs`), an ACP prompt (which reaches the same submit over
RPC), and `kj drive --prompt` (`kj/drive.rs`). A drive without a prompt, a
tool round, and a turn resumed by a completion carry none.

The notice is a `(System, Notification)` block authored by the kernel, so it
hydrates as a kernel instruction and never reads as a person's words
(`llm/hydrate.rs`, `is_kernel_instruction`). Its insert and the reset of
every watch it names are one acceptance (`BlockStore::insert_watch_notice`).
A reset that finds a count other than the one rendered fails the delivery,
so a change is never lost between render and reset. A prompt with no waiting
change adds no notice.

## Listening

The kernel runs one host file watcher (`path_watch.rs`) on `notify`. It
watches each watched file's parent directory, so a save that writes a new
file and renames it over the old one is seen. It records events on notify's
own thread, off the beat path. Reads and metadata changes do not count.

The database decides who an event reaches: only live contexts' watches
accrue. The watcher's directory set follows `kj watch add`, `kj watch rm`, and
kernel start; an archived or stopped watch the watcher still listens for
records nothing.
A watcher that cannot start at kernel start is logged as an error and the
kernel keeps running; `kj watch add` reports the failure.

## Lifecycle

- **Rotation restarts watches.** The successor gets a copy of each watch with
  an empty status. The predecessor's rows stay with it and are archived with
  it, so old changes do not build up in the successor.
- **A fork copies no watches.**
- **Promote does not re-arm.** `kj context promote` stops the watches of a
  context it brings back from the archive (`stopped_at`), in the same
  transaction. `kj watch list` shows them as stopped, and `kj watch add` on
  the same path starts the watch again with no changes.
