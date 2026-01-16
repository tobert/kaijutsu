use bevy::prelude::*;

/// Tracks the currently selected item in the context area
#[derive(Resource, Default)]
pub struct NavigationState {
    /// Index of selected message (None = nothing selected)
    pub selected: Option<usize>,
}

impl NavigationState {
    pub fn select_next(&mut self, max: usize) {
        if max == 0 {
            self.selected = None;
            return;
        }
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) => (i + 1).min(max - 1),
        });
    }

    pub fn select_prev(&mut self) {
        self.selected = match self.selected {
            None => None,
            Some(0) => Some(0),
            Some(i) => Some(i - 1),
        };
    }
}
