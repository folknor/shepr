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

pub use launch::{MANAGED_AGENT_RESUME_TIMEOUT, PaneLaunchEnv, PaneShellConfig};
pub use runtime::WheelRouting;
pub use runtime::{PaneOutputWrite, PaneOutputWriter, PaneRuntime};
pub use runtime_registry::PaneRuntimeRegistry;
pub use state::PaneState;
pub use teardown::PaneTeardownTracker;
#[cfg(test)]
use terminal::GhosttyPaneTerminal;
pub use terminal::{PaneClearError, ScrollMetrics, TerminalCursorState};
pub use terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalSearchDirection, TerminalSearchWindow,
    TerminalTextPoint, TerminalWordMotion,
};
