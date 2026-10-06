//! The pane lifecycle events: spawn and exit.

pub(super) fn pane_spawn_started(
    pane_id: shepr_core::layout::PaneId,
    rows: u16,
    cols: u16,
    scrollback: shepr_core::scrollback::ScrollbackBudget,
    kind: super::launch::LaunchKind,
    cwd: &shepr_core::absolute_path::AbsolutePath,
    shell: &std::path::Path,
) {
    tracing::info!(
        event = "pane.spawn.start",
        subsystem = "pane",
        outcome = "started",
        pane = %pane_id,
        kind = ?kind,
        cwd = %cwd.display(),
        shell = %shell.display(),
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
        pane = %pane_id,
        pid = pid.get(),
        "pane child spawned"
    );
}

pub(super) fn pane_exited(
    pane_id: shepr_core::layout::PaneId,
    exit_status: &std::process::ExitStatus,
) {
    let status = exit_status.to_string();
    // The child reserves this status for a failed setup step before exec. A
    // shell can exit with it too, so the field says what the status means to
    // shepr, not that setup certainly failed.
    let setup_failure_status = exit_status.code() == Some(shepr_pty::backend::EXIT_SETUP_FAILED);
    tracing::info!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "completed",
        pane = %pane_id,
        status = status.as_str(),
        setup_failure_status,
        "pane child exited"
    );
}

pub(super) fn pane_exit_failed(pane_id: shepr_core::layout::PaneId, err: &str) {
    tracing::error!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "error",
        pane = %pane_id,
        error = err,
        "pane child wait failed"
    );
}
