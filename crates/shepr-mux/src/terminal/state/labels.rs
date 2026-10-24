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

    /// Sets or clears the agent's directory name; true when it changed.
    pub fn set_agent_label(&mut self, label: Option<Label>) -> bool {
        if self.agent_label == label {
            return false;
        }
        self.agent_label = label;
        true
    }
}
