use std::path::Path;

pub(crate) fn startup() {
    // The PID is event identity for correlating each process's lifecycle rows.
    tracing::info!(
        event = "app.startup",
        subsystem = "server",
        outcome = "started",
        pid = std::process::id(),
        "shepr starting"
    );
}

pub(crate) fn shutdown() {
    tracing::info!(
        event = "app.shutdown",
        subsystem = "server",
        outcome = "completed",
        pid = std::process::id(),
        "shepr exiting"
    );
}

pub(crate) fn workspace_created(
    workspace_id: &shepr_protocol::WorkspaceId,
    root_pane_id: shepr_core::layout::PaneId,
) {
    tracing::info!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "ok",
        %workspace_id,
        pane_id = %root_pane_id,
        "workspace created"
    );
}

pub(crate) fn workspace_focused(workspace_id: &shepr_protocol::WorkspaceId) {
    tracing::info!(
        event = "workspace.focus",
        subsystem = "workspace",
        outcome = "ok",
        %workspace_id,
        "workspace focused"
    );
}

pub(crate) fn workspace_closed(workspace_id: &shepr_protocol::WorkspaceId) {
    tracing::info!(
        event = "workspace.close",
        subsystem = "workspace",
        outcome = "ok",
        %workspace_id,
        "workspace closed"
    );
}

pub(crate) fn workspace_renamed(workspace_id: &shepr_protocol::WorkspaceId) {
    tracing::info!(
        event = "workspace.rename",
        subsystem = "workspace",
        outcome = "ok",
        %workspace_id,
        "workspace renamed"
    );
}

pub(crate) fn session_restored(path: &Path, summary: shepr_mux::persist::SessionRestoreSummary) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome = summary.outcome.as_log_value(),
        path = %path.display(),
        workspaces = summary.workspaces,
        "session restore evaluated"
    );
}
