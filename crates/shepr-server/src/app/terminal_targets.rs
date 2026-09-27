use super::{App, api_helpers::pane_agent_status};
use shepr_protocol::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTarget {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: shepr_core::layout::PaneId,
    pub terminal_id: TerminalId,
}

#[derive(Clone, Copy)]
struct TerminalTargetRef<'a> {
    ws_idx: usize,
    tab_idx: usize,
    pane_id: shepr_core::layout::PaneId,
    terminal_id: &'a TerminalId,
}

impl TerminalTargetRef<'_> {
    fn into_owned(self) -> TerminalTarget {
        TerminalTarget {
            ws_idx: self.ws_idx,
            tab_idx: self.tab_idx,
            pane_id: self.pane_id,
            terminal_id: self.terminal_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Terminal,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalTargetCandidate {
    pub terminal_id: TerminalId,
    pub pane_id: shepr_protocol::PublicPaneId,
    pub workspace_id: shepr_protocol::WorkspaceId,
    pub tab_id: shepr_protocol::PublicTabId,
    pub cwd: Option<String>,
    pub agent_status: shepr_api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalTargetError {
    NotFound {
        target: String,
    },
    Ambiguous {
        target: String,
        candidates: Vec<TerminalTargetCandidate>,
    },
}

impl App {
    pub(crate) fn resolve_terminal_target(
        &self,
        target: &str,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        self.resolve_target(target, TargetKind::Terminal)
    }

    pub(crate) fn resolve_agent_target(
        &self,
        target: &str,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        self.resolve_target(target, TargetKind::Agent)
    }

    fn resolve_target(
        &self,
        target: &str,
        kind: TargetKind,
    ) -> Result<TerminalTarget, TerminalTargetError> {
        if kind == TargetKind::Terminal {
            let terminal_matches = self
                .terminal_targets()
                .filter(|candidate| candidate.terminal_id.as_str() == target);
            if let Some(resolved) = self.single_terminal_match(target, terminal_matches)? {
                return Ok(resolved);
            }
        }

        if let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(target)
            && let Some(resolved) = self.terminal_target_for_pane(ws_idx, pane_id)
            && (kind == TargetKind::Terminal || self.target_is_agent(&resolved))
        {
            return Ok(resolved);
        }

        let name_matches = self.terminal_targets().filter(|candidate| {
            self.state
                .terminals
                .get(candidate.terminal_id)
                .is_some_and(|terminal| match kind {
                    TargetKind::Terminal => {
                        terminal.agent_name.as_deref() == Some(target)
                            || terminal.effective_agent_label() == Some(target)
                    }
                    TargetKind::Agent => terminal.agent_name.as_deref() == Some(target),
                })
        });
        if let Some(resolved) = self.single_terminal_match(target, name_matches)? {
            return Ok(resolved);
        }

        Err(TerminalTargetError::NotFound {
            target: target.to_string(),
        })
    }

    fn target_is_agent(&self, target: &TerminalTarget) -> bool {
        self.state
            .terminals
            .get(&target.terminal_id)
            .is_some_and(crate::terminal::TerminalState::is_agent_terminal)
    }

    fn single_terminal_match<'a>(
        &self,
        target: &str,
        matches: impl IntoIterator<Item = TerminalTargetRef<'a>>,
    ) -> Result<Option<TerminalTarget>, TerminalTargetError> {
        let mut matches = matches.into_iter();
        let Some(first) = matches.next() else {
            return Ok(None);
        };
        let Some(second) = matches.next() else {
            return Ok(Some(first.into_owned()));
        };
        let candidates = std::iter::once(first)
            .chain(std::iter::once(second))
            .chain(matches)
            .filter_map(|candidate| {
                self.terminal_target_candidate(candidate.ws_idx, candidate.pane_id)
            })
            .collect();
        Err(TerminalTargetError::Ambiguous {
            target: target.to_string(),
            candidates,
        })
    }

    fn terminal_targets(&self) -> impl Iterator<Item = TerminalTargetRef<'_>> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().enumerate().flat_map(move |(tab_idx, tab)| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| {
                            tab.terminal_id(pane_id)
                                .map(|terminal_id| TerminalTargetRef {
                                    ws_idx,
                                    tab_idx,
                                    pane_id,
                                    terminal_id,
                                })
                        })
                })
            })
    }

    fn terminal_target_for_pane(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<TerminalTarget> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let terminal_id = ws.terminal_id(pane_id)?.clone();
        Some(TerminalTarget {
            ws_idx,
            tab_idx,
            pane_id,
            terminal_id,
        })
    }

    fn terminal_target_candidate(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<TerminalTargetCandidate> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        let pane_number = ws.public_pane_number(pane_id)?;
        let tab_number = ws.public_tab_number(tab_idx)?;
        Some(TerminalTargetCandidate {
            terminal_id: terminal.id.clone(),
            pane_id: shepr_protocol::PublicPaneId::new(&ws.id, pane_number),
            workspace_id: shepr_protocol::WorkspaceId::new(ws.id.clone()),
            tab_id: shepr_protocol::PublicTabId::new(&ws.id, tab_number),
            cwd: ws.tabs[tab_idx]
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            agent_status: pane_agent_status(terminal.state),
        })
    }
}
