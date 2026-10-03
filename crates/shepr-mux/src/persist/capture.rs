use std::collections::HashMap;
use std::path::Path;

use crate::pane::PaneRuntimeRegistry;
use crate::terminal::TerminalState;
use crate::workspace::Workspace;
use shepr_protocol::TerminalId;

use super::actor::{PersistJob, SessionBundle};
use super::snapshot::{SavedPaneRef, capture_deferred, capture_pending_history};

/// Captures the current session, clearing its saved state when no workspace
/// remains and otherwise writing one structural snapshot with optional pane
/// history. The returned terminal index uses the same pane keys as the snapshot.
pub fn capture_job(
    workspaces: &[Workspace],
    terminals: &HashMap<TerminalId, TerminalState>,
    terminal_runtimes: &PaneRuntimeRegistry,
    fallback_cwd: &Path,
    active: Option<usize>,
    host_theme: shepr_term::host::TerminalTheme,
    persist_pane_history: bool,
) -> (PersistJob, HashMap<SavedPaneRef, TerminalId>) {
    if workspaces.is_empty() {
        return (PersistJob::Clear, HashMap::new());
    }
    let (snapshot, cwds, terminal_ids) = capture_deferred(
        workspaces,
        terminals,
        terminal_runtimes,
        fallback_cwd,
        active,
        host_theme,
    );
    let history =
        persist_pane_history.then(|| capture_pending_history(workspaces, terminal_runtimes));
    (
        PersistJob::Save(SessionBundle {
            snapshot,
            cwds,
            history,
        }),
        terminal_ids,
    )
}
