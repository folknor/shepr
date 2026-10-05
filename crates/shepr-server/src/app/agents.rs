use super::App;
use super::api::session::SnapshotAgent;

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<SnapshotAgent> {
        self.state
            .workspaces
            .iter()
            .flat_map(|ws| {
                ws.tree()
                    .pane_ids()
                    .into_iter()
                    .filter_map(move |pane_id| self.state.pane(pane_id))
                    .filter_map(move |pane| self.agent_info(&pane))
            })
            .collect()
    }

    pub(super) fn agent_info(
        &self,
        pane: &shepr_mux::workspace::PaneRef<'_>,
    ) -> Option<SnapshotAgent> {
        let terminal = pane.terminal();
        let ownership = terminal.ownership();
        let agent = ownership.effective_agent()?;
        Some(SnapshotAgent {
            pane_id: pane.public_id(),
            workspace_id: pane.workspace().id(),
            agent,
            terminal_title: terminal.terminal_title().map(str::to_owned),
            terminal_title_stripped: terminal.terminal_title_stripped(),
            agent_status: super::api_helpers::pane_agent_status(ownership.state()),
            state_change_seq: ownership
                .last_agent_state_change_seq()
                .unwrap_or(shepr_agent::StateChangeSeq::NEVER),
        })
    }
}
