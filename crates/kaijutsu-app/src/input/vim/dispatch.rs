//! Vim dispatch system for compose input.
//!
//! When the compose overlay is focused, keyboard events are routed through
//! the VimMachine instead of the flat binding table. This system converts
//! Bevy keyboard events to TerminalKey, feeds them to the state machine,
//! and translates the resulting modalkit Actions into kaijutsu ActionFired
//! or TextInputReceived messages.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;

use editor_types::{
    Action as MkAction, EditAction, EditorAction, InsertTextAction, PromptAction,
};
use editor_types::context::Resolve;
use editor_types::prelude::{Char, EditTarget, Specifier};
use modalkit::editing::context::EditContext;
use modalkit::keybindings::BindingMachine;

use super::keyconv::bevy_to_terminal_key;
use super::motion::{self, MotionContext};
use super::{KaijutsuAction, KaijutsuInfo, VimMachineResource};
use crate::input::action::Action;
use crate::input::events::{ActionFired, TextInputReceived};

/// Per-overlay vim motion state. Stored alongside VimMachineResource.
/// Tracks desired_col for j/k column memory.
#[derive(Resource, Default)]
pub struct VimMotionState {
    pub desired_col: Option<usize>,
}

/// Bevy system that dispatches keyboard input through the VimMachine
/// when the compose overlay is focused.
///
/// Run condition: `in_compose()` — only active when FocusArea::Compose.
pub fn vim_dispatch_compose(
    mut keyboard: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    mut vim: ResMut<VimMachineResource>,
    mut motion_state: ResMut<VimMotionState>,
    mut overlay: Query<&mut crate::cell::InputOverlay>,
    mut action_writer: MessageWriter<ActionFired>,
    mut text_writer: MessageWriter<TextInputReceived>,
) {
    let Ok(mut overlay) = overlay.single_mut() else {
        return;
    };

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Ctrl+C bypasses VimMachine — interrupt must always work
        // regardless of vim state (operator-pending, visual, etc.).
        if (keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight))
            && event.key_code == KeyCode::KeyC
        {
            action_writer.write(ActionFired(Action::InterruptContext { immediate: false }));
            continue;
        }

        let Some(tkey) = bevy_to_terminal_key(event, &keys) else {
            continue;
        };

        // Filter key repeats for non-insert modes to prevent accidental
        // double-fires (same rationale as the flat dispatch system).
        // In insert mode, key repeat is fine (holding backspace, typing).
        if event.repeat && vim.machine.show_mode().is_none() {
            // show_mode() returns None in Normal mode
            continue;
        }

        vim.machine.input_key(tkey);

        // Drain all produced actions
        while let Some((mk_action, ctx)) = vim.machine.pop() {
            translate_action(
                &mk_action,
                &ctx,
                &mut overlay,
                &mut motion_state,
                &mut action_writer,
                &mut text_writer,
            );
        }
    }
}

/// Translate a modalkit Action into overlay mutations, ActionFired, or TextInputReceived.
fn translate_action(
    action: &MkAction<KaijutsuInfo>,
    ctx: &EditContext,
    overlay: &mut crate::cell::InputOverlay,
    motion_state: &mut VimMotionState,
    action_writer: &mut MessageWriter<ActionFired>,
    text_writer: &mut MessageWriter<TextInputReceived>,
) {
    match action {
        // --- Insert mode: character typing ---
        MkAction::Editor(EditorAction::InsertText(InsertTextAction::Type(spec, _dir, _count))) => {
            if let Specifier::Exact(ch) = spec {
                match ch {
                    Char::Single(c) => {
                        text_writer.write(TextInputReceived(c.to_string()));
                    }
                    Char::Digraph(a, b) => {
                        log::debug!("vim: digraph ({}, {}) not yet supported", a, b);
                    }
                    _ => {
                        log::debug!("vim: unsupported Char variant: {:?}", ch);
                    }
                }
            }
        }

        // --- Normal mode: motion (cursor movement only, no editing) ---
        MkAction::Editor(EditorAction::Edit(op_spec, EditTarget::Motion(move_type, count))) => {
            let resolved_op: EditAction = ctx.resolve(op_spec);
            if matches!(resolved_op, EditAction::Motion) {
                let motion_ctx = MotionContext {
                    desired_col: motion_state.desired_col,
                };
                let result = motion::resolve_motion(
                    &overlay.text,
                    overlay.cursor,
                    move_type,
                    count,
                    ctx,
                    &motion_ctx,
                );
                overlay.cursor = result.cursor.min(overlay.text.len());
                motion_state.desired_col = result.desired_col;
                // Clear selection on motion in Normal mode
                overlay.selection_anchor = None;
            } else {
                // Phase 4: delete/yank/change with motion target
                log::trace!("vim: edit op {:?} with motion not yet handled", resolved_op);
            }
        }

        // --- Submit (Enter with submit_on_enter) ---
        MkAction::Prompt(PromptAction::Submit) => {
            action_writer.write(ActionFired(Action::Submit));
        }

        // --- Prompt abort (Ctrl+D when empty, or Escape in command mode) ---
        MkAction::Prompt(PromptAction::Abort(..)) => {
            action_writer.write(ActionFired(Action::Unfocus));
        }

        // --- Application-specific actions ---
        MkAction::Application(app_action) => match app_action {
            KaijutsuAction::Submit => {
                action_writer.write(ActionFired(Action::Submit));
            }
            KaijutsuAction::CycleModeRing => {
                action_writer.write(ActionFired(Action::CycleModeRing));
            }
            KaijutsuAction::DismissCompose => {
                action_writer.write(ActionFired(Action::Unfocus));
            }
        },

        // --- Editor actions not yet handled (Phase 4: delete, yank, change, etc.) ---
        MkAction::Editor(_) => {
            log::trace!("vim: editor action not yet handled: {:?}", action);
        }

        // --- NoOp ---
        MkAction::NoOp => {}

        // --- Everything else: log for now ---
        _ => {
            log::trace!("vim: unhandled action: {:?}", action);
        }
    }
}
