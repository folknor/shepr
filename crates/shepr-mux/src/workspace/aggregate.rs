use std::collections::HashMap;

use crate::terminal::TerminalState;
use shepr_agent::detect::AgentState;
use shepr_protocol::TerminalId;

use super::{Tab, Workspace};

fn aggregate_attention(panes: impl Iterator<Item = AgentState>) -> AgentState {
    panes
        .max_by_key(|state| state.attention_rank())
        .unwrap_or(AgentState::Unknown)
}

fn pane_states<'a>(
    tab: &'a Tab,
    terminals: &'a HashMap<TerminalId, TerminalState>,
) -> impl Iterator<Item = AgentState> + 'a {
    tab.panes.values().filter_map(|pane| {
        terminals
            .get(&pane.attached_terminal_id)
            .map(|terminal| terminal.state)
    })
}

impl Tab {
    /// Aggregate agent state of this tab's panes; see `Workspace::aggregate_state`.
    pub fn aggregate_state(&self, terminals: &HashMap<TerminalId, TerminalState>) -> AgentState {
        aggregate_attention(pane_states(self, terminals))
    }
}

impl Workspace {
    /// Aggregate agent state over every pane in every tab, preferring Blocked,
    /// then Working, then Idle.
    pub fn aggregate_state(&self, terminals: &HashMap<TerminalId, TerminalState>) -> AgentState {
        aggregate_attention(self.tabs.iter().flat_map(|tab| pane_states(tab, terminals)))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Direction;

    use shepr_core::layout::PaneId;

    use super::*;

    fn terminal_for_pane(ws: &Workspace, pane_id: PaneId) -> TerminalState {
        TerminalState::new(
            ws.terminal_id(pane_id).expect("test precondition").clone(),
            "/shepr-aggregate-test".into(),
        )
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");
        let mut terminals = HashMap::new();
        let root = ws.tabs[0].root_pane;
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);

        assert_eq!(ws.aggregate_state(&terminals), AgentState::Unknown);
    }

    #[test]
    fn aggregate_state_priority_is_blocked_then_working_then_idle() {
        for states in [
            [AgentState::Idle, AgentState::Working, AgentState::Blocked],
            [AgentState::Blocked, AgentState::Idle, AgentState::Working],
        ] {
            assert_eq!(aggregate_attention(states.into_iter()), AgentState::Blocked);
        }
    }

    #[test]
    fn tab_aggregate_state_covers_only_its_own_panes() {
        let mut ws = Workspace::test_new("test");
        let first_root = ws.tabs[0].root_pane;
        let second_tab = ws.test_add_tab(None);
        let second_root = ws.tabs[second_tab].root_pane;
        let mut terminals = HashMap::new();
        let mut blocked = terminal_for_pane(&ws, first_root);
        blocked.state = AgentState::Blocked;
        terminals.insert(blocked.id.clone(), blocked);
        let mut working = terminal_for_pane(&ws, second_root);
        working.state = AgentState::Working;
        terminals.insert(working.id.clone(), working);

        assert_eq!(
            ws.tabs[second_tab].aggregate_state(&terminals),
            AgentState::Working
        );
        assert_eq!(ws.aggregate_state(&terminals), AgentState::Blocked);
    }

    #[test]
    fn blocked_state_beats_other_panes_in_a_split() {
        let mut ws = Workspace::test_new("test");
        let second = ws.test_split(Direction::Horizontal);
        let first = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != second)
            .copied()
            .expect("test precondition");
        let mut terminals = HashMap::new();
        let mut idle = terminal_for_pane(&ws, first);
        idle.state = AgentState::Idle;
        terminals.insert(idle.id.clone(), idle);
        let mut blocked = terminal_for_pane(&ws, second);
        blocked.state = AgentState::Blocked;
        terminals.insert(blocked.id.clone(), blocked);

        assert_eq!(ws.tabs[0].aggregate_state(&terminals), AgentState::Blocked);
    }
}
