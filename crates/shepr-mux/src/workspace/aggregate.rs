use shepr_agent::PresentedAgentState;

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
    pub fn aggregate_state(&self) -> PresentedAgentState {
        aggregate_attention(
            self.tree
                .panes()
                .map(|(_, record)| record.terminal().ownership().state().presentation_state()),
        )
    }
}

#[cfg(test)]
mod tests {
    use shepr_agent::AgentState;
    use shepr_core::layout::{Direction, PaneId};

    use super::*;

    fn set_state(ws: &mut Workspace, pane: PaneId, state: AgentState) {
        ws.pane_mut(pane)
            .expect("test precondition")
            .terminal_mut()
            .ownership_mut()
            .set_detected_state_with_screen_signals_at(
                None,
                state,
                false,
                false,
                std::time::Instant::now(),
            );
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");

        assert_eq!(ws.aggregate_state(), PresentedAgentState::Idle);
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
        set_state(&mut ws, second, AgentState::Idle);

        assert_eq!(ws.aggregate_state(), PresentedAgentState::Idle);
    }

    #[test]
    fn blocked_state_beats_other_panes_in_a_split() {
        let mut ws = Workspace::test_new("test");
        let first = ws.tree().root();
        let second = ws.test_split(Direction::Horizontal);
        set_state(&mut ws, first, AgentState::Idle);
        set_state(&mut ws, second, AgentState::Blocked);

        assert_eq!(ws.aggregate_state(), PresentedAgentState::Blocked);
    }

    #[test]
    fn working_state_beats_idle_in_a_split() {
        let mut ws = Workspace::test_new("test");
        let first = ws.tree().root();
        let second = ws.test_split(Direction::Horizontal);
        set_state(&mut ws, first, AgentState::Working);
        set_state(&mut ws, second, AgentState::Idle);

        assert_eq!(ws.aggregate_state(), PresentedAgentState::Working);
    }
}
