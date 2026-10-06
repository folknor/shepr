pub(crate) fn startup() {
    // The PID is event identity for correlating each process's lifecycle rows.
    shepr_platform::structured_log!(
        INFO,
        event = client.startup,
        outcome = "started",
        pid = std::process::id(),
        "shepr starting"
    );
}

pub(crate) fn shutdown() {
    shepr_platform::structured_log!(
        INFO,
        event = client.shutdown,
        outcome = "completed",
        pid = std::process::id(),
        "shepr exiting"
    );
}
