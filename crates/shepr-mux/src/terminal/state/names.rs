use super::*;

impl TerminalState {
    pub fn set_manual_label(&mut self, mut label: String) {
        let trimmed = label.trim();
        if trimmed.len() != label.len() {
            label = trimmed.to_string();
        }
        self.manual_label = (!label.is_empty()).then_some(label);
    }

    pub fn clear_manual_label(&mut self) {
        self.manual_label = None;
    }

    pub fn is_agent_terminal(&self) -> bool {
        self.ownership.has_agent()
    }
}
