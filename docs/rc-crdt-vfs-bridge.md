# RC Scripts on the CRDT-VFS

Status: **shipped (increments 1–3).** rc scripts are now files under
`/etc/rc`, read by dispatch via `FileDocumentCache`; the `rc_scripts` table,
`RcScriptRow`, and per-script timeout are gone. `file:write`/`edit` under
`/etc/rc` is gated on the dedicated `Capability::RcWrite` (deny-by-default,
*not* implied by `*`/`facade:*`); the rest of `/etc` is denied flat. Host
`vim` and `kj rc` are unaffected. Supersedes the earlier A/B/C analysis.

Per the shared-trust model this is an **ergonomic nudge**, not a security
wall: the kernel runs with the user's full rights, so the gate exists to
make the deliberate path (a role granted `rc-write`, or `kj rc`) easier than
the accidental one (a `coder` clobbering a privileged lifecycle script).

## Goal

Back rc lifecycle scripts (`/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`)
with **real files** instead of a SQLite `rc_scripts` table, so they can be:

- edited with `vim` on the host (and, later, co-edited live in-app via the CRDT),
- read/inspected as ordinary files through the same path we'd edit any code,
- reviewed in the repo as the default `/etc/rc` image.

Non-goal: changing *what* rc scripts do or *when* they fire. Purely a
storage / edit-surface change underneath the existing dispatch.

## Decisions (locked)

1. **Files are the single source of truth.** No `rc_scripts` table. Dispatch
   reads the files directly (via `FileDocumentCache`, mtime-cached). There is
   exactly one durable representation of script content — the file — so the
   row-vs-CRDT divergence the old options worried about cannot occur. The CRDT
   is the edit/merge layer every file already has; the (future) in-app editor
   and host-side `vim` both land in the same place.

2. **Storage layout.**
   - **Repo seed source:** `assets/defaults/rc/**`, a 1:1 mirror of `/etc/rc`,
     embedded at build time (`include_str!`). This is the "floor."
   - **Deployed tree:** `~/.config/kaijutsu/rc/**` (XDG config, beside
     `models.toml` et al.), mounted into the VFS at `/etc/rc` as a
     **read-write** `LocalBackend`. Longest-prefix wins over the read-only `/`
     mount, so the **host's real `/etc` is never touched**.
   - On boot, embedded seeds are written to the deployed tree **if absent**
     (the `config_backend` floor pattern: write-default-if-missing). A deleted
     file reappears next boot; a user edit persists; `kj rc reseed` overwrites.

3. **Write gate (deny-by-default).** `/etc/rc` is a real rw VFS path, but the
   kernel's `builtin.file:write` / `file:edit` tools are **refused** under
   `/etc/**` unless the calling context's loadout explicitly grants an
   `rc-write` capability. rc scripts run *privileged on every fork*, so an
   open writable `/etc/rc` would hand privileged persistence to any context
   holding `file:write` (e.g. `coder`) — a prompt-injection escalation. Host
   `vim` is unaffected (it bypasses the kernel guard entirely), so the
   "edit the real file" goal holds regardless of the in-kernel gate.

4. **No per-script metadata columns.** The path encodes
   `context_type / verb / sort_key / name / ext`. `created_by` is dropped —
   provenance comes from the CRDT block's `principal_id`. `created_at` is
   dropped. **Per-script `--timeout` is removed**; all `.kai` run under the
   kernel default (`kaish.timeouts().rc_script_timeout`). A `kj` knob can
   re-add a per-script override later (frontmatter or sidecar).

## Why this over the old "row is truth, CRDT projects" sketch

Keeping the row authoritative meant you could never `vim` the canonical copy
— it lived in a SQLite blob. Making the **file** canonical delivers the
stated goal directly and reuses an already-proven pattern (`config_backend`:
embed default → snap to `~/.config` → file-watch → CRDT). The only cost is
dispatch reading files on the fork hot path instead of an indexed row; the
files are tiny, OS-cached, and behind `FileDocumentCache`'s mtime cache.

## Existing infrastructure this reuses (code is truth)

- **`FileDocumentCache`** (`file_tools/cache.rs`): VFS path → CRDT doc
  (`uuid_v5` of path) → write-through `flush_one` → `MountTable.write_all` →
  `LocalBackend` → host disk. mtime-stale reload picks up external `vim`
  edits; dirty-edits-win never clobbers in-flight work. Owned by `Kernel`
  (`file_cache`), reachable from rc/lifecycle via `kernel().file_cache(blocks)`.
- **VFS mounts** (`rpc.rs`): `/` = `LocalBackend::read_only("/")`, `~/src` rw,
  `/tmp` rw, then `freeze_mounts()`. The `/etc/rc` rw mount is added before
  the freeze.
- **Capability allow-set** (`mcp/binding.rs`): one `allows(&Capability)`
  predicate; `Admin` is the template for a new unit `RcWrite` capability,
  grantable via the binding string `"rc-write"`.

## Increment plan

- **1 — Asset extraction (shipped).** Seed bodies moved from inline Rust
  consts into `assets/defaults/rc/**`, embedded via `include_str!`. Behavior
  unchanged (still seeds the table); the files are now vim-able. Commit
  `refactor(rc): extract seed bodies to assets/defaults/rc files`.

- **2 — Files-as-truth (shipped).** The atomic DB→files switch:
  - Mount `~/.config/kaijutsu/rc` → `/etc/rc` rw before freeze
    (`rpc.rs`); seed-to-disk floor on boot; `kj rc reseed` rewrites files.
  - `run_rc_lifecycle` (`kj/lifecycle.rs`): `readdir /etc/rc/<type>/<verb>/`,
    sort by filename, read each via `FileDocumentCache`. `.md` → block;
    `.kai` → kaish with the kernel-default timeout.
  - `kj rc add/edit/rm/show/list` operate through the cache, not rows
    (`kj/rc.rs`, now async).
  - Removed the `rc_scripts` table, `RcScriptRow`, CRUD, and per-script
    timeout. One-time migration: legacy rows → files on first boot
    (`KernelDb::legacy_rc_scripts`, no data loss). Tests rewritten against a
    seeded `/etc/rc` mount in `test_dispatcher`.
  - Write policy is the **safe baseline**: `file:write`/`edit` under `/etc`
    is denied for everyone (`file_tools/path.rs::deny_etc_write`); only admin
    `kj rc` (direct cache use) and host `vim` can edit. Non-divergent.

- **3 — `rc-write` capability (shipped).** Added the dedicated
  `Capability::RcWrite` + `"rc-write"` binding token (mirrors `Admin`:
  persisted as the `binding_rc_write` column, not implied by `*`/`facade:*`).
  The file engines gate `/etc/rc` writes on it via
  `WorkspaceGuard::context_allows_rc_write`; `is_rc_path` separates the rc
  tree from the flat `/etc` deny. `coder` stays locked out by default; a
  trusted role opts in with `kj binding allow rc-write`. An additive
  `ALTER TABLE ... ADD COLUMN` backfills pre-existing DBs (no wipe needed).

## Open questions (carried forward)

- **Edit-while-fork.** A peer editing `S00-stance.md` while another forks a
  `coder` context: dispatch runs the last-flushed content. Leans acceptable.
- **`.kai` validation.** Should the edit surface pre-validate kaish syntax
  before a body becomes dispatch-visible? kaish already pre-validates at run;
  a broken `.kai` lands an Error block, not a silent failure. Probably enough.
- **DTE / cross-peer replication.** rc docs are CRDT docs; do they replicate
  over the same drift/DTE path as conversation docs? Ties into the
  "Config CRDT ops" issue.
- **Eviction.** rc docs are tiny and hot at fork; consider pinning them
  (exempt from the cache LRU) so a fork never pays a cold reload.
