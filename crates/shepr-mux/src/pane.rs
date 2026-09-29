mod agent_detection;
mod cursor;
mod launch;
mod osc;
mod process_probe;
mod runtime;
mod runtime_registry;
mod state;
mod teardown;
mod terminal;

pub use launch::{PaneLaunchEnv, PaneShellConfig};
pub use runtime::PaneCwdProbe;
pub use runtime::WheelRouting;
pub use runtime::{PaneOutputWrite, PaneOutputWriter, PaneRuntime};
pub use runtime_registry::PaneRuntimeRegistry;
pub use state::PaneState;
pub use teardown::PaneTeardownTracker;
pub use terminal::{PaneClearError, ScrollMetrics, TerminalCursorState};
pub use terminal::{PaneHistoryCache, PaneHistorySource};
pub use terminal::{
    TerminalDirtyPatch, TerminalDirtyPatchOutcome, TerminalSearchDirection, TerminalSearchWindow,
    TerminalTextPoint, TerminalWordMotion,
};

#[cfg(test)]
use terminal::PaneTerminal;
