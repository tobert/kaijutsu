//! Embedded scripting runtime.
//!
//! Hosts the kaish executor, the kaish↔kernel backends, and the per-session
//! context registry. Lives in `kaijutsu-kernel` (not `kaijutsu-server`) so the
//! kernel can run kaish scripts on its own initiative — context lifecycle (rc)
//! hooks, `HookBody::Kaish`, distillation, scheduled drift, etc. — without
//! needing a connected SSH session.
//!
//! - `embedded_kaish` — the kaish kernel embedded in-process.
//! - `kaish_backend` — bridges kaish ops to kaijutsu kernel state.
//! - `mount_backend` — kaish's filesystem root, wires VFS mounts to kaish.
//! - `docs_filesystem` — `/v/docs` (kernel blocks-as-files).
//! - `input_filesystem` — `/v/input` (the compose draft block).
//! - `read_only_fs` — read-only wrapper for the `/v/*` document mounts (toolie).
//! - `context_engine` — per-session "current context" registry.
//! - `kj_builtin` — the `kj` kaish Tool.

pub mod config_doc_fs;
pub mod context_engine;
pub mod docs_filesystem;
pub mod embedded_kaish;
pub mod input_filesystem;
pub mod kaish_backend;
pub mod kj_builtin;
pub mod mount_backend;
pub mod read_only_fs;
pub mod synthesis;
pub mod vi_builtin;
