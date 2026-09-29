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
            agent_metadata: HashMap::new(),
            metadata_tokens: crate::terminal::metadata_tokens::MetadataTokens::default(),
            persisted_agent_session: None,
            terminal_title: None,
            manual_label: None,
            agent_name: None,
            agent_name_owner: None,
            resume_name_hold: None,
            hook_report_sequences: HashMap::new(),
            hook_report_accepted_at: HashMap::new(),
            suppressed_full_lifecycle_hook_reports: HashMap::new(),
            stale_full_lifecycle_hook_sessions: HashMap::new(),
            metadata_report_sequences: HashMap::new(),
            metadata_report_agents: HashMap::new(),
            metadata_token_sequence_sources: std::collections::HashSet::new(),
            state: AgentState::Unknown,
            last_agent_state_change_seq: None,
            revision: 0,
            launch_argv: None,
            recent_agent_process_exit: None,
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
