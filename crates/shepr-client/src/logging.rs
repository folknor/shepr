pub(crate) fn startup() {
    // The PID is event identity for correlating each process's lifecycle rows.
    tracing::info!(
        event = "app.startup",
        subsystem = "client",
        outcome = "started",
        pid = std::process::id(),
        "shepr starting"
    );
}

pub(crate) fn shutdown() {
    tracing::info!(
        event = "app.shutdown",
        subsystem = "client",
        outcome = "completed",
        pid = std::process::id(),
        "shepr exiting"
    );
}
