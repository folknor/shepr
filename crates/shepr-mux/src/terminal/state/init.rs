use super::*;
use shepr_core::absolute_path::AbsolutePath;

impl TerminalState {
    /// `cwd` is the directory the pane was launched in, or a restored pane's
    /// saved path. Absolute is all it promises: a saved directory that has
    /// disappeared is kept so a later restore can retry it, and the live cwd
    /// reported by the pane supersedes it through `set_cwd`.
    pub fn new(cwd: AbsolutePath) -> Self {
        Self {
            cwd,
            terminal_title: None,
            manual_label: None,
            ownership: AgentOwnership::new(),
            agent_resume: AgentResumeState::None,
            start_failure: None,
        }
    }

    pub fn cwd(&self) -> &AbsolutePath {
        &self.cwd
    }

    /// A reported cwd arrives as a `UsableCwd`, an observed existing
    /// directory, and is kept as the absolute path it wraps.
    pub fn set_cwd(&mut self, cwd: crate::UsableCwd) {
        self.cwd = cwd.into_absolute();
    }
}
