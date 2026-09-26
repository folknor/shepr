use std::collections::HashMap;

use crate::detect::AgentState;
use crate::terminal::{TerminalId, TerminalState};

use super::Workspace;

fn pane_attention_priority(state: AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (AgentState::Blocked, _) => 4,
        (AgentState::Idle, false) => 3,
        (AgentState::Working, _) => 2,
        (AgentState::Idle, true) => 1,
        (AgentState::Unknown, _) => 0,
    }
}

impl Workspace {
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> (AgentState, bool) {
        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.values())
            .filter_map(|pane| {
                terminals
                    .get(&pane.attached_terminal_id)
                    .map(|terminal| (terminal.state, pane.seen))
            })
            // Panes iterate in `HashMap` order, so every tie must be broken
            // inside the key: among equal priorities prefer the unseen pane.
            // With that, two panes only tie when (state, seen) are identical
            // and the result no longer depends on iteration order.
            .max_by_key(|(state, seen)| (pane_attention_priority(*state, *seen), !*seen))
            .unwrap_or((AgentState::Unknown, true))
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
            }
        }
    }
}
