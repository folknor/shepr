/// Text read from a pane's rows, and whether rows above the first one returned
/// were left out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalReadSnapshot {
    pub text: String,
    pub truncated: bool,
}
