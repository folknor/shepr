mod agent_detection;
mod cursor;
mod cwd;
mod launch;
mod osc;
mod process_probe;
mod runtime;
mod state;
mod teardown;
mod terminal;

pub(crate) use launch::{MANAGED_AGENT_RESUME_TIMEOUT, PANE_TERM, PaneLaunchEnv, PaneShellConfig};
pub use runtime::PaneRuntime;
pub(crate) use runtime::WheelRouting;
pub use state::PaneState;
pub(crate) use teardown::wait_for_pane_session_teardowns;
#[cfg(test)]
use terminal::GhosttyPaneTerminal;
pub use terminal::{ScrollMetrics, TerminalCursorState};
pub(crate) use terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalReadSnapshot, TerminalSearchDirection,
    TerminalSearchWindow, TerminalTextPoint, TerminalWordMotion,
};
