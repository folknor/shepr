pub(crate) fn startup() {
    // The PID is event identity for correlating each process's lifecycle rows.
    shepr_platform::structured_log!(
        INFO,
        event = server.startup,
        outcome = Started,
        pid = std::process::id(),
        "shepr starting"
    );
}

pub(crate) fn shutdown() {
    shepr_platform::structured_log!(
        INFO,
        event = server.shutdown,
        outcome = Ok,
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
    shepr_platform::structured_log!(
        INFO, event = workspace.create, outcome = Ok,
        %workspace_id,
        public_pane_id = %root_pane_id,
        cwd = %cwd.as_path().display(),
        name,
        "workspace created"
    );
}

pub(crate) fn workspace_focused(workspace_id: &shepr_protocol::WorkspaceId) {
    shepr_platform::structured_log!(
        INFO, event = workspace.focus, outcome = Ok,
        %workspace_id,
        "workspace focused"
    );
}

pub(crate) fn workspace_closed(workspace_id: &shepr_protocol::WorkspaceId) {
    shepr_platform::structured_log!(
        INFO, event = workspace.close, outcome = Ok,
        %workspace_id,
        "workspace closed"
    );
}

pub(crate) fn workspace_renamed(workspace_id: &shepr_protocol::WorkspaceId, name: &str) {
    shepr_platform::structured_log!(
        INFO, event = workspace.rename, outcome = Ok,
        name,
        %workspace_id,
        "workspace renamed"
    );
}

// Pane fields: `public_pane_id` is the id the operator sees (`w1:p1`); a
// process-local `PaneId` is logged as `pane`. No key carries both.

pub(crate) fn pane_removed(pane_id: shepr_protocol::PublicPaneId) {
    shepr_platform::structured_log!(
        INFO, event = pane.remove, outcome = Ok,
        public_pane_id = %pane_id, "pane lifecycle changed"
    );
}

pub(crate) fn pane_split(pane_id: shepr_protocol::PublicPaneId) {
    shepr_platform::structured_log!(
        INFO, event = pane.split, outcome = Ok,
        public_pane_id = %pane_id, "pane lifecycle changed"
    );
}

pub(crate) fn pane_zoomed(pane_id: shepr_protocol::PublicPaneId, zoomed: bool) {
    shepr_platform::structured_log!(INFO, event = pane.zoom, outcome = Ok, public_pane_id = %pane_id, zoomed, "pane zoom changed");
}

pub(crate) fn panes_swapped(
    source: shepr_protocol::PublicPaneId,
    target: shepr_protocol::PublicPaneId,
) {
    shepr_platform::structured_log!(INFO, event = pane.swap, outcome = Ok, public_pane_id = %source, swapped_with = %target, "panes swapped");
}

pub(crate) fn workspace_moved(
    workspace_id: &shepr_protocol::WorkspaceId,
    before: Option<&shepr_protocol::WorkspaceId>,
) {
    shepr_platform::structured_log!(INFO, event = workspace.move, outcome = Ok, %workspace_id, ?before, "workspace moved");
}

pub(crate) fn creation_refused(
    workspace_id: &shepr_protocol::WorkspaceId,
    kind: &str,
    reason: &impl std::fmt::Debug,
) {
    shepr_platform::structured_log!(ERROR, event = workspace.create, outcome = Refused, %workspace_id, kind, error = ?reason, "creation refused");
}
