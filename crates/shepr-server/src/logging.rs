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
    root_pane_id: shepr_protocol::PublicPaneId,
    cwd: &shepr_core::absolute_path::AbsolutePath,
    name: &str,
) {
    tracing::info!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "ok",
        %workspace_id,
        public_pane_id = %root_pane_id,
        cwd = %cwd.as_path().display(),
        name,
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

pub(crate) fn workspace_renamed(workspace_id: &shepr_protocol::WorkspaceId, name: &str) {
    tracing::info!(
        name,
        event = "workspace.rename",
        subsystem = "workspace",
        outcome = "ok",
        %workspace_id,
        "workspace renamed"
    );
}

// Pane fields: `public_pane_id` is the id the operator sees (`w1:p1`); a
// process-local `PaneId` is logged as `pane`. No key carries both.

pub(crate) fn pane_event(event: &str, pane_id: shepr_protocol::PublicPaneId) {
    tracing::info!(event, subsystem = "pane", outcome = "ok", public_pane_id = %pane_id, "pane lifecycle changed");
}

pub(crate) fn pane_zoomed(pane_id: shepr_protocol::PublicPaneId, zoomed: bool) {
    tracing::info!(event = "pane.zoom", subsystem = "pane", outcome = "ok", public_pane_id = %pane_id, zoomed, "pane zoom changed");
}

pub(crate) fn panes_swapped(
    source: shepr_protocol::PublicPaneId,
    target: shepr_protocol::PublicPaneId,
) {
    tracing::info!(event = "pane.swap", subsystem = "pane", outcome = "ok", public_pane_id = %source, swapped_with = %target, "panes swapped");
}

pub(crate) fn workspace_moved(
    workspace_id: &shepr_protocol::WorkspaceId,
    before: Option<&shepr_protocol::WorkspaceId>,
) {
    tracing::info!(event = "workspace.move", subsystem = "workspace", outcome = "ok", %workspace_id, ?before, "workspace moved");
}

pub(crate) fn creation_refused(
    workspace_id: &shepr_protocol::WorkspaceId,
    kind: &str,
    reason: &impl std::fmt::Debug,
) {
    tracing::error!(event = "workspace.creation_refused", subsystem = "workspace", outcome = "refused", %workspace_id, kind, ?reason, "creation refused");
}
