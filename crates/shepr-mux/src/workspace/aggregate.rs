use std::collections::HashMap;

use crate::terminal::TerminalState;
use shepr_agent::detect::PresentedAgentState;
use shepr_protocol::TerminalId;

use super::Workspace;

fn aggregate_attention(panes: impl Iterator<Item = PresentedAgentState>) -> PresentedAgentState {
    panes
        .max_by_key(|state| state.attention_rank())
        .unwrap_or(PresentedAgentState::Idle)
}

impl Workspace {
    /// Aggregate agent state over every pane, preferring Blocked, then
    /// Working, then Idle. Unknown has already been presented as Idle, so the
    /// result cannot depend on which equal-attention pane a HashMap visits last.
    pub fn aggregate_state(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> PresentedAgentState {
        aggregate_attention(self.panes.values().filter_map(|pane| {
            terminals
                .get(&pane.attached_terminal_id)
                .map(|terminal| terminal.ownership().state().presentation_state())
        }))
    }
}

#[cfg(test)]
mod tests {
    use shepr_agent::detect::AgentState;
    use shepr_core::layout::{Direction, PaneId};

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
        let root = ws.root_pane;
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);

        assert_eq!(ws.aggregate_state(&terminals), PresentedAgentState::Idle);
    }

    #[test]
    fn aggregate_state_priority_is_blocked_then_working_then_idle() {
        for states in [
            [
                PresentedAgentState::Idle,
                PresentedAgentState::Working,
                PresentedAgentState::Blocked,
            ],
            [
                PresentedAgentState::Blocked,
                PresentedAgentState::Idle,
                PresentedAgentState::Working,
            ],
        ] {
            assert_eq!(
                aggregate_attention(states.into_iter()),
                PresentedAgentState::Blocked
            );
        }
    }

    #[test]
    fn aggregate_state_collapses_unknown_and_idle_before_aggregation() {
        let mut ws = Workspace::test_new("test");
        let second = ws.test_split(Direction::Horizontal);
        let first = ws
            .panes
            .keys()
            .find(|id| **id != second)
            .copied()
            .expect("test precondition");
        let mut terminals = HashMap::new();
        let unknown = terminal_for_pane(&ws, first);
        terminals.insert(unknown.id.clone(), unknown);
        let mut idle = terminal_for_pane(&ws, second);
        idle.ownership_mut()
            .set_detected_state_with_screen_signals_at(
                None,
                AgentState::Idle,
                false,
                false,
                std::time::Instant::now(),
            );
        terminals.insert(idle.id.clone(), idle);

        assert_eq!(ws.aggregate_state(&terminals), PresentedAgentState::Idle);
    }

    #[test]
    fn blocked_state_beats_other_panes_in_a_split() {
        let mut ws = Workspace::test_new("test");
        let second = ws.test_split(Direction::Horizontal);
        let first = ws
            .panes
            .keys()
            .find(|id| **id != second)
            .copied()
            .expect("test precondition");
        let mut terminals = HashMap::new();
        let mut idle = terminal_for_pane(&ws, first);
        idle.ownership_mut()
            .set_detected_state_with_screen_signals_at(
                None,
                AgentState::Idle,
                false,
                false,
                std::time::Instant::now(),
            );
        terminals.insert(idle.id.clone(), idle);
        let mut blocked = terminal_for_pane(&ws, second);
        blocked
            .ownership_mut()
            .set_detected_state_with_screen_signals_at(
                None,
                AgentState::Blocked,
                false,
                false,
                std::time::Instant::now(),
            );
        terminals.insert(blocked.id.clone(), blocked);

        assert_eq!(ws.aggregate_state(&terminals), PresentedAgentState::Blocked);
    }
}
