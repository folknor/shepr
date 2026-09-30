//! Launch flow for `shepr` with no subcommand: attach to the local server,
//! starting it as a detached daemon first when none is listening.

use std::io;
use std::time::Duration;

/// Checks the local server, starts it when needed, then runs the client. The
/// caller has already rejected an unusable terminal
/// ([`ensure_terminal_geometry`]) before preflight and before this runs.
///
/// A running server of a different build fails the launch with guidance for
/// the resolved socket target. The startup step before this one
/// (`preflight::run`) has already offered to restart it; what reaches here is a
/// server the operator kept, or one that could not be asked about. With machines configured, a
/// local startup failure does not end the launch, so the remote machines stay
/// reachable. It is still refused, not swallowed: the failure is printed to
/// stderr before the TUI takes the terminal, where it is on screen again once
/// the TUI exits, and the Local endpoint's handshake reports a build mismatch
/// with the same guidance as its status in the sidebar.
///
/// A launch failure before the client runs is the error; once the client has
/// run, its own result is handed back untouched for the caller to report.
pub(crate) fn auto_detect_launch<T>(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    server_ready_timeout: Duration,
    run_client: impl FnOnce(&shepr_config::ValidatedConfig, &shepr_config::AppPaths) -> T,
) -> io::Result<T> {
    let socket_path = paths.server_address().client_socket().to_path_buf();
    tracing::info!(path = %socket_path.display(), "auto-detect launch starting");

    // The running server is checked whether or not machines are
    // configured. With configured machines a mismatch does not end the launch below,
    // so they stay reachable; the Local endpoint's own handshake then rejects
    // the different build and shows the same guidance.
    let startup = shepr_remote::local_server::ensure_running(
        paths,
        server_ready_timeout,
        shepr_remote::local_server::BuildCheck::BeforeAttach,
    );
    if let Err(error) = startup {
        if config.machines().is_empty() {
            return Err(error);
        }
        // Keep the full refusal visible even though the client will remain open
        // for the configured machines; the endpoint state omits this startup detail.
        crate::cli::print_notice(&local_startup_notice(&error));
    }

    Ok(run_client(config, paths))
}

/// Rejects a TUI launch with no usable terminal geometry before preflight can
/// authenticate machines or offer to restart a server.
pub(crate) fn ensure_terminal_geometry() -> io::Result<()> {
    shepr_platform::terminal_grid_size()
        .map(|_| ())
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot attach without a usable terminal: {err}; run inside a terminal"),
            )
        })
}

/// What the operator is told when Local fails to start or is refused while
/// configured machines keep the client running.
fn local_startup_notice(error: &io::Error) -> String {
    format!("shepr: Local is unavailable; configured machines stay available.\n{error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal reaches the operator whole, guidance included, instead of
    /// a log line nothing receives.
    #[test]
    fn the_local_startup_notice_carries_the_whole_refusal() {
        let error = io::Error::other(format!(
            "the running shepr server is a different build.\n\n{}",
            "To use this build here instead, stop the running server. Run the profile-specific `server stop` command, then run this build again."
        ));
        let notice = local_startup_notice(&error);
        assert!(
            notice.contains("configured machines stay available"),
            "{notice}"
        );
        assert!(notice.contains("server stop"), "{notice}");
        assert!(!notice.contains("--force"), "{notice}");
    }
}
