//! The pane lifecycle events: spawn and exit.

pub(super) fn pane_spawn_started(
    pane_id: shepr_core::layout::PaneId,
    rows: u16,
    cols: u16,
    scrollback: shepr_core::scrollback::ScrollbackBudget,
) {
    tracing::info!(
        event = "pane.spawn.start",
        subsystem = "pane",
        outcome = "started",
        %pane_id,
        rows,
        cols,
        scrollback_limit_bytes = scrollback.bytes(),
        "spawning pane terminal"
    );
}

pub(super) fn pane_spawned(pane_id: shepr_core::layout::PaneId, pid: shepr_platform::Pid) {
    tracing::info!(
        event = "pane.spawned",
        subsystem = "pane",
        outcome = "ok",
        %pane_id,
        pid = pid.get(),
        "pane child spawned"
    );
}

pub(super) fn pane_exited(
    pane_id: shepr_core::layout::PaneId,
    exit_status: &std::process::ExitStatus,
) {
    let status = exit_status.to_string();
    tracing::info!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "completed",
        %pane_id,
        status = status.as_str(),
        "pane child exited"
    );
}

pub(super) fn pane_exit_failed(pane_id: shepr_core::layout::PaneId, err: &str) {
    tracing::error!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "error",
        %pane_id,
        error = err,
        "pane child wait failed"
    );
}
