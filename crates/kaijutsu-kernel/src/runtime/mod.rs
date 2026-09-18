//! Contextual command execution and model turns.
//!
//! `prompt` persists interactive input; `turn_request` owns preparation for
//! interactive and headless turns. `approval_resume` delivers answered
//! asks and executes captured approved source. `llm_stream` owns model turns;
//! `turn_state` owns their conversations and interrupts. `context_shell` owns contextual construction and builtin wiring.
//! `command` owns captured execution, result review, and block-pair settlement;
//! `structured` owns addressed kj invocation; `interactive` owns shell submission;
//! `editor_read` owns complete-text editor shell reads.
//! `streaming` owns cancellable transport commands. `command_outcome` retains execution
//! and hook results. `command_result` and `shell_state` supply shared projections
//! and durable write-back.
//! `embedded_kaish` owns the interpreter and its execution adapters. Backend,
//! filesystem, and builtin modules implement kaish interfaces. Rc orchestration
//! remains a distinct owner; see `docs/kaish-integration.md`.

pub mod admission;
pub mod rc_lifecycle;
pub mod prompt;
pub mod turn_request;
pub mod approval_resume;
pub(crate) mod completion_notice;
pub mod turn_state;
pub mod interrupt;
pub mod llm_stream;
mod turn_identity;
pub mod command;
pub mod structured;
pub mod interactive;
pub mod streaming;
pub(crate) mod editor_read;
pub(crate) mod tool_command;
pub(crate) mod worker;
pub mod command_result;
pub mod command_outcome;
mod result_review;
pub mod shell_state;
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
