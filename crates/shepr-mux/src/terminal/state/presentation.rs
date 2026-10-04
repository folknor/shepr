use super::*;

impl TerminalState {
    pub fn border_label(&self, show_agent_labels: bool) -> Option<Label> {
        self.manual_label.clone().or_else(|| {
            show_agent_labels
                .then(|| {
                    self.ownership
                        .effective_agent()
                        .and_then(|agent| Label::new(agent.label()))
                })
                .flatten()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_agent::Agent;

    #[test]
    fn border_label_prefers_manual_label_over_agent_label() {
        let mut terminal = TerminalState::new("/".into());
        terminal
            .ownership_mut()
            .set_detected_state_with_screen_signals_at(
                Some(Agent::Claude),
                AgentState::Idle,
                false,
                false,
                Instant::now(),
            );

        assert_eq!(terminal.border_label(false), None);
        assert_eq!(
            terminal.border_label(true).as_ref().map(Label::as_str),
            Some("claude")
        );

        terminal.set_manual_label(" reviewer ".into());
        assert_eq!(
            terminal.border_label(false).as_ref().map(Label::as_str),
            Some("reviewer")
        );
        assert_eq!(
            terminal.border_label(true).as_ref().map(Label::as_str),
            Some("reviewer")
        );

        terminal.set_manual_label("   ".into());
        assert_eq!(
            terminal.border_label(true).as_ref().map(Label::as_str),
            Some("claude")
        );

        terminal.set_manual_label("reviewer".into());
        terminal.clear_manual_label();
        assert_eq!(
            terminal.border_label(true).as_ref().map(Label::as_str),
            Some("claude")
        );
    }
}
