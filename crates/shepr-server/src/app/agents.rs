use super::App;
use super::api::session::SnapshotAgent;

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<SnapshotAgent> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.layout()
                    .pane_ids()
                    .into_iter()
                    .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
            })
            .collect()
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<SnapshotAgent> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        Some(SnapshotAgent {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            workspace_id: self.public_workspace_id(ws_idx)?,
            agent: terminal
                .ownership()
                .effective_agent_label()
                .map(str::to_string),
            terminal_title: terminal.terminal_title().map(str::to_owned),
            terminal_title_stripped: terminal.terminal_title_stripped(),
            agent_status: super::api_helpers::pane_agent_status(terminal.ownership().state()),
            state_change_seq: terminal
                .ownership()
                .last_agent_state_change_seq()
                .unwrap_or(0),
        })
    }
}
