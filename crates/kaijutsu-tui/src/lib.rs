//! `kaijutsu-tui` — the terminal client.
//!
//! A standalone binary that dials the kernel over the existing SSH RPC
//! subsystem and takes the alternate screen for the session: the transcript
//! is a view over the current context's blocks, the band sits under it.
//! It is not a kernel subsystem and not a Bevy frontend
//! (`docs/tui.md`, "Roads not taken").
//!
//! ```text
//! terminal
//!    │  keys + cells
//! kaijutsu-tui            ratatui over the alternate screen
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
//! | [`layout`] | structured tool output (`OutputData`) → width-aware lines: tables, `ls -C` columns, trees |
//! | [`app`] | contexts, mirrors, rank, asks, cache health, notices |
//! | [`status`] | the status line's figures and its layout |
//! | [`render`] | the `Backend`-generic renderer: the transcript, the band, the overlays |
//! | [`keys`] | key events → intents, including the `Ctrl+A` prefix |
//! | [`run`] | the event loop |
//! | [`compose`] | the vi surface over the context's kernel-owned draft, and the `:` bar |
//! | [`cmdline`] | the `:` line's own dialect: `:kj`, `:!`, `:q` |
//! | [`interrupt`] | the `Ctrl+C` escalation ladder |
//! | [`asks`] | the ask card, the ledger view (`Ctrl+A l`) |
//! | [`inflight`] | the in-flight strip: one fixed row naming unsettled tool calls |
//! | [`completion`] | `:kj ` completion over the `kj` command catalog |
//! | [`picker`] | the seat picker (`Ctrl+A "`), drawn as an overlay |
//! | [`editor`], [`diff`] | the two full-screen surfaces |
//! | [`copy`] | the transcript off its live tail: motions, mark, search, yank |

pub mod app;
pub mod asks;
pub mod bridge;
pub mod cmdline;
pub mod compose;
pub mod completion;
pub mod copy;
pub mod inflight;
pub mod interrupt;
pub mod keys;
pub mod layout;
pub mod picker;
pub mod refresh;
pub mod present;
pub mod render;
pub mod run;
pub mod status;
pub mod diff;
pub mod editor;
