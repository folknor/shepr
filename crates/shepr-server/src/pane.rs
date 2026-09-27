mod agent_detection;
mod cursor;
mod cwd;
mod launch;
mod osc;
mod process_probe;
mod runtime;
mod runtime_registry;
mod state;
mod teardown;
mod terminal;

pub use launch::SHEPR_PANE_ID_ENV_VAR;
pub(crate) use launch::{MANAGED_AGENT_RESUME_TIMEOUT, PaneLaunchEnv, PaneShellConfig};
pub use runtime::PaneRuntime;
pub(crate) use runtime::WheelRouting;
pub(crate) use runtime_registry::PaneRuntimeRegistry;
pub use state::PaneState;
pub(crate) use teardown::wait_for_pane_session_teardowns;
#[cfg(test)]
use terminal::GhosttyPaneTerminal;
pub use terminal::{PaneClearError, ScrollMetrics, TerminalCursorState};
pub(crate) use terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalSearchDirection, TerminalSearchWindow,
    TerminalTextPoint, TerminalWordMotion,
};
