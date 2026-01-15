use bevy::prelude::*;

/// The sacred input buffer - what the user is currently typing
#[derive(Resource, Default)]
pub struct InputBuffer {
    /// Committed text
    pub text: String,
    /// IME preedit (composing) text - shown but not yet committed
    pub preedit: String,
}

impl InputBuffer {
    pub fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }

    pub fn pop(&mut self) {
        self.text.pop();
    }

    pub fn clear(&mut self) -> String {
        self.preedit.clear();
        std::mem::take(&mut self.text)
    }

    pub fn set_preedit(&mut self, s: &str) {
        self.preedit = s.to_string();
    }

    pub fn clear_preedit(&mut self) {
        self.preedit.clear();
    }

    pub fn display(&self) -> String {
        if self.text.is_empty() && self.preedit.is_empty() {
            "> _".to_string()
        } else if self.preedit.is_empty() {
            format!("> {}▏", self.text)
        } else {
            // Show preedit in a distinct style (underlined effect via unicode)
            format!("> {}[{}]▏", self.text, self.preedit)
        }
    }
}
