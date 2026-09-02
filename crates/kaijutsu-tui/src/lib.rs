//! `kaijutsu-tui` — the terminal client.
//!
//! A standalone binary that dials the kernel over the existing SSH RPC
//! subsystem and renders an inline ratatui viewport: the transcript flows
//! into the terminal's own scrollback, the live UI sits at the bottom.
//! It is not a kernel subsystem and not a Bevy frontend
//! (`docs/tui.md`, "Roads not taken").
//!
//! ```text
//! terminal
//!    │  keys + cells
//! kaijutsu-tui            ratatui inline viewport
//!    │  kaijutsu-client   ActorHandle, ContextMirror, ServerEvent
//! kaijutsu-server / kernel
//! ```
//!
//! The three-part split is `kaijutsu-acp`'s: [`bridge`] speaks Cap'n Proto,
//! [`present`] is the pure mapper (no RPC, no I/O, no clock), and [`render`]
//! is the edge. [`app`] holds the state both sides read, and it is pure too.
//!
//! | Module | Surface |
//! |---|---|
//! | [`bridge`] | connect, hydrate a context, submit a turn |
//! | [`present`] | `BlockSnapshot` → styled lines, the wrap cache, the palette |
//! | [`app`] | contexts, mirrors, rank, asks, cache health, notices |
//! | [`status`] | the status line's figures and its layout |
//! | [`render`] | the `Backend`-generic renderer and the inline viewport |
//! | [`keys`] | key events → intents, including the `Ctrl+A` prefix |
//! | [`run`] | the event loop |
//! | [`compose`], [`shell`], [`picker`], [`asks`] | later lanes |
//! | [`editor`], [`diff`] | the two alternate-screen surfaces |

pub mod app;
pub mod asks;
pub mod bridge;
pub mod compose;
pub mod keys;
pub mod picker;
pub mod present;
pub mod render;
pub mod run;
pub mod shell;
pub mod status;
pub mod diff;
pub mod editor;
