mod agent_detection;
mod agent_osc;
mod child_watcher;
mod cursor;
mod detect;
mod detection_task;
mod exit_arbiter;
mod launch;
mod launch_status;
mod logging;
mod osc7;
mod osc_debug;
mod process_probe;
mod runtime;
mod runtime_registry;
mod teardown;
mod terminal;

pub use exit_arbiter::{PaneEndReason, PaneEnding};
pub use launch::{LaunchKind, PaneShellConfig, init_pane_launches};
pub use launch_status::{LaunchOutcome, LaunchSettlement};
pub use runtime::PaneCwdProbe;
pub use runtime::{LaunchPresentation, PaneLaunchRequest, PaneLauncher, PaneSpawnHandles};
pub use runtime::{PaneOutputWrite, PaneOutputWriter, PaneRead, PaneRuntime};
pub use runtime_registry::PaneRuntimeRegistry;
pub use teardown::PaneTeardownTracker;
pub use terminal::AgentDetectionInputs;
pub use terminal::{ContentRevision, DetectionSeq, SyncEpoch, SyncState};
pub use terminal::{
    CursorRead, PaneClearError, PaneDraw, ScrollMetrics, TerminalCursorState, WheelRouting,
};
pub use terminal::{HistoryPiece, HistoryUnavailable, PaneHistoryCache, PaneHistorySource};
pub use terminal::{
    PatchFallback, PatchRow, PatchUnavailable, TerminalCopyMotion, TerminalCopyMotionError,
    TerminalDirtyPatch, TerminalDirtyPatchSnapshot, TerminalLineMotion, TerminalParagraphMotion,
    TerminalSearchCase, TerminalSearchDirection, TerminalSearchLimit, TerminalSearchPosition,
    TerminalSearchWindow, TerminalTextPoint, TerminalTextRange, TerminalTextSearch,
    TerminalWordMotion,
};

#[cfg(test)]
use terminal::PaneTerminal;
