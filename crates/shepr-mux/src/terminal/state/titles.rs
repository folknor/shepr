use super::*;

impl TerminalState {
    pub fn terminal_title_stripped(&self) -> Option<String> {
        self.terminal_title
            .as_deref()
            .and_then(crate::terminal::stripped_terminal_title)
    }

    pub fn set_terminal_title(&mut self, title: Option<String>) -> TerminalTitleChange {
        if self.terminal_title == title {
            return TerminalTitleChange::default();
        }
        let previous_stripped = self.terminal_title_stripped();
        self.terminal_title = title;
        TerminalTitleChange {
            raw_changed: true,
            stripped_changed: previous_stripped != self.terminal_title_stripped(),
        }
    }
}
