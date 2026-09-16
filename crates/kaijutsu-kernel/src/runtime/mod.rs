//! Shared kaish integration for command, lifecycle, hook, and editor consumers.
//!
//! `context_shell` owns contextual construction and builtin wiring.
//! `embedded_kaish` owns the interpreter and its execution adapters. Backend,
//! filesystem, and builtin modules implement kaish interfaces. Rc orchestration
//! remains a distinct owner; see `docs/kaish-integration.md`.

pub mod context_engine;
pub mod context_shell;
pub mod curl_tool;
pub mod docs_filesystem;
pub mod embedded_kaish;
pub mod kaish_backend;
pub mod kj_builtin;
pub mod ps_builtin;
pub mod mount_backend;
pub mod read_only_fs;
pub mod swap_filesystem;
pub mod synthesis;
pub mod vi_builtin;
