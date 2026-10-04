use super::*;

impl TerminalState {
    /// `cwd` is the directory the pane was launched in, or a restored pane's
    /// saved absolute path. It is recorded as given: a saved directory that has
    /// disappeared is kept so a later restore can retry it, and the live cwd
    /// reported by the pane supersedes it through `set_cwd`.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            terminal_title: None,
            manual_label: None,
            ownership: AgentOwnership::new(),
            agent_resume: AgentResumeState::None,
            restore_error: None,
        }
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Every later write goes through `UsableCwd`, so an unchecked path cannot
    /// replace the launch cwd.
    pub fn set_cwd(&mut self, cwd: crate::UsableCwd) {
        self.cwd = cwd.into_path_buf();
    }
}
