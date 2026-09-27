use shepr_protocol::TerminalId;

/// Viewport state for a pane.
///
/// Terminal identity, cwd, labels, and agent metadata live in TerminalState.
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
