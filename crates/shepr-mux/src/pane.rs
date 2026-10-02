mod agent_detection;
mod child_watcher;
mod cursor;
mod detection_task;
mod exit_arbiter;
mod launch;
mod launch_status;
mod osc;
mod process_probe;
mod runtime;
mod runtime_registry;
mod state;
mod teardown;
mod terminal;

pub use launch::{PaneLaunchEnv, PaneShellConfig, init_pane_launches};
pub use launch_status::LaunchSettlement;
pub use runtime::PaneCwdProbe;
pub use runtime::WheelRouting;
pub use runtime::{PaneOutputWrite, PaneOutputWriter, PaneRuntime};
pub use runtime_registry::PaneRuntimeRegistry;
pub use state::PaneState;
pub use teardown::PaneTeardownTracker;
pub use terminal::AgentDetectionInputs;
pub use terminal::{HistoryPiece, PaneHistoryCache, PaneHistorySource};
pub use terminal::{PaneClearError, ScrollMetrics, TerminalCursorState};
pub use terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalSearchDirection, TerminalSearchWindow,
    TerminalTextPoint, TerminalWordMotion,
};

#[cfg(test)]
use terminal::PaneTerminal;
