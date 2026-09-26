use std::collections::HashMap;

use crate::detect::AgentState;
use crate::terminal::{TerminalId, TerminalState};

use super::{Tab, Workspace};

fn pane_attention_priority(state: AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (AgentState::Blocked, _) => 4,
        (AgentState::Idle, false) => 3,
        (AgentState::Working, _) => 2,
        (AgentState::Idle, true) => 1,
        (AgentState::Unknown, _) => 0,
    }
}

/// The `(state, seen)` of the pane most in need of attention among `panes`,
/// or `(Unknown, true)` when there are none.
fn aggregate_attention(panes: impl Iterator<Item = (AgentState, bool)>) -> (AgentState, bool) {
    panes
        // Panes iterate in `HashMap` order, so every tie must be broken
        // inside the key: among equal priorities prefer the unseen pane.
        // With that, two panes only tie when (state, seen) are identical
        // and the result no longer depends on iteration order.
        .max_by_key(|(state, seen)| (pane_attention_priority(*state, *seen), !*seen))
        .unwrap_or((AgentState::Unknown, true))
}

fn pane_states<'a>(
    tab: &'a Tab,
    terminals: &'a HashMap<TerminalId, TerminalState>,
) -> impl Iterator<Item = (AgentState, bool)> + 'a {
    tab.panes.values().filter_map(|pane| {
        terminals
            .get(&pane.attached_terminal_id)
            .map(|terminal| (terminal.state, pane.seen))
    })
}

impl Tab {
    /// Aggregate agent state of this tab's panes; see `Workspace::aggregate_state`.
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        aggregate_attention(pane_states(self, terminals))
    }
}

impl Workspace {
    /// Aggregate agent state over every pane in every tab: the most urgent
    /// state, and whether the pane carrying it has been seen.
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        aggregate_attention(self.tabs.iter().flat_map(|tab| pane_states(tab, terminals)))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Direction;

    use crate::layout::PaneId;

    use super::*;

    fn terminal_for_pane(ws: &Workspace, pane_id: PaneId) -> TerminalState {
        TerminalState::new(
            ws.terminal_id(pane_id).expect("test precondition").clone(),
            "/tmp".into(),
        )
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");
        let mut terminals = HashMap::new();
        let root = ws.tabs[0].root_pane;
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);
        let (state, seen) = ws.aggregate_state(&terminals);
        assert_eq!(state, AgentState::Unknown);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_priority() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .expect("test precondition");
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Working);
        assert!(seen);
    }

    #[test]
    fn aggregate_state_done_unseen_beats_working() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .expect("test precondition");
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);
        let root = ws.tabs[0]
            .panes
            .get_mut(&root_id)
            .expect("test precondition");
        root.seen = false;

        let (state, seen) = ws.aggregate_state(&terminals);

        assert_eq!(state, AgentState::Idle);
        assert!(!seen);
    }

    #[test]
    fn aggregate_state_prefers_unseen_among_equal_priority_regardless_of_order() {
        for state in [AgentState::Blocked, AgentState::Working] {
            for unseen_first in [false, true] {
                let mut ws = Workspace::test_new("test");
                let id2 = ws.test_split(Direction::Horizontal);
                let root_id = ws.tabs[0]
                    .panes
                    .keys()
                    .find(|id| **id != id2)
                    .copied()
                    .expect("test precondition");
                let mut terminals = HashMap::new();
                for pane_id in [root_id, id2] {
                    let mut terminal = terminal_for_pane(&ws, pane_id);
                    terminal.state = state;
                    terminals.insert(terminal.id.clone(), terminal);
                }
                let unseen = if unseen_first { root_id } else { id2 };
                ws.tabs[0]
                    .panes
                    .get_mut(&unseen)
                    .expect("test precondition")
                    .seen = false;

                assert_eq!(ws.aggregate_state(&terminals), (state, false));
                assert_eq!(ws.tabs[0].aggregate_state(&terminals), (state, false));
            }
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
            (AgentState::Working, true)
        );
        assert_eq!(ws.aggregate_state(&terminals), (AgentState::Blocked, true));
    }
}
