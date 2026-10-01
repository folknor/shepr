use super::*;

impl TerminalState {
    /// `cwd` is the directory the pane was launched in, or a restored pane's
    /// saved absolute path. It is recorded as given: a saved directory that has
    /// disappeared is kept so a later restore can retry it, and the live cwd
    /// reported by the pane supersedes it through `set_cwd`.
    pub fn new(id: TerminalId, cwd: PathBuf) -> Self {
        Self {
            id,
            cwd,
            detected_agent: None,
            fallback_state: AgentState::Unknown,
            fallback_visible_blocker: false,
            fallback_observed_at: None,
            hook_authority: None,
            persisted_agent_session: None,
            terminal_title: None,
            manual_label: None,
            hook_sources: HashMap::new(),
            state: AgentState::Unknown,
            last_agent_state_change_seq: None,
            process_evidence: AgentProcessEvidence::default(),
            pending_agent_resume_plan: None,
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
