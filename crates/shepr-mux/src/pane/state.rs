use shepr_protocol::TerminalId;

/// Per-pane state: its terminal link and input flags.
///
/// Terminal identity, cwd, labels, and agent state live in TerminalState.
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    /// Whether unmodified right-click gestures should be forwarded to the pane application.
    pub right_click_passthrough: bool,
}

impl PaneState {
    pub fn new(attached_terminal_id: TerminalId) -> Self {
        Self {
            attached_terminal_id,
            right_click_passthrough: false,
        }
    }
}
