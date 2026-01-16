use bevy::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, States, Hash)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Command,
}

impl Mode {
    pub fn indicator(&self) -> &'static str {
        match self {
            Mode::Normal => "[N]",
            Mode::Insert => "[I]",
            Mode::Command => "[:]",
        }
    }
}

pub fn handle_mode_input(
    keys: Res<ButtonInput<KeyCode>>,
    current: Res<State<Mode>>,
    mut next: ResMut<NextState<Mode>>,
) {
    match current.get() {
        Mode::Normal => {
            if keys.just_pressed(KeyCode::KeyI) {
                next.set(Mode::Insert);
            }
            if keys.just_pressed(KeyCode::Semicolon) && keys.pressed(KeyCode::ShiftLeft) {
                next.set(Mode::Command);
            }
        }
        Mode::Insert | Mode::Command => {
            if keys.just_pressed(KeyCode::Escape) {
                next.set(Mode::Normal);
            }
        }
    }
}
