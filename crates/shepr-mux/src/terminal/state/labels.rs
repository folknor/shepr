use crate::Label;

use super::TerminalState;

impl TerminalState {
    /// Stores a label its caller validated at its input boundary.
    pub fn set_manual_label(&mut self, label: Label) {
        self.manual_label = Some(label);
    }

    pub fn clear_manual_label(&mut self) {
        self.manual_label = None;
    }
}
