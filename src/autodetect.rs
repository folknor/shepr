//! Launch flow for `shepr` with no subcommand: attach to the local server,
//! starting it as a detached daemon first when none is listening.

use std::io;
use std::time::Duration;

/// Checks the local server, starts it when needed, then runs the client.
///
/// A running server of a different build fails the launch with guidance for
/// the resolved socket target. With saved machines configured, a
/// local startup failure does not end the launch, so the remote machines stay
/// reachable. It is still refused, not swallowed: the failure is printed to
/// stderr before the TUI takes the terminal, where it is on screen again once
/// the TUI exits, and the Local endpoint's handshake reports a build mismatch
/// with the same guidance as its status in the sidebar.
///
/// A launch failure before the client runs is the error; once the client has
/// run, its own result is handed back untouched for the caller to report.
pub(crate) fn auto_detect_launch<T>(
    endpoint_catalog: shepr_remote::machine::EndpointCatalog,
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    server_ready_timeout: Duration,
    run_client: impl FnOnce(
        &shepr_config::ValidatedConfig,
        &shepr_config::AppPaths,
        shepr_remote::machine::EndpointCatalog,
    ) -> T,
) -> io::Result<T> {
    // The client requires terminal geometry before it can attach. Reject an
    // unusable terminal before socket lookup creates directories or starts a daemon.
    shepr_platform::terminal_grid_size().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot attach without a usable terminal: {err}; run inside a terminal"),
        )
    })?;
    let socket_path = paths.server_address().client_socket().to_path_buf();
    tracing::info!(path = %socket_path.display(), "auto-detect launch starting");

    // The running server is checked whether or not saved machines are
    // enabled. With saved machines a mismatch does not end the launch below,
    // so they stay reachable; the Local endpoint's own handshake then rejects
    // the different build and shows the same guidance.
    let startup = shepr_remote::local_server::ensure_running(
        paths,
        server_ready_timeout,
        shepr_remote::local_server::BuildCheck::BeforeAttach,
    );
    if let Err(error) = startup {
        if !endpoint_catalog.has_ssh() {
            return Err(error);
        }
        // Keep the full refusal visible even though the client will remain open
        // for saved machines; the endpoint state omits this startup detail.
        crate::cli::print_notice(&local_startup_notice(&error));
    }

    Ok(run_client(config, paths, endpoint_catalog))
}

/// What the operator is told when Local fails to start or is refused while
/// saved machines keep the client running.
fn local_startup_notice(error: &io::Error) -> String {
    format!("shepr: Local is unavailable; saved machines stay available.\n{error}")
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
            shepr_api::guidance::operator_guidance(
                shepr_api::guidance::OperatorGuidance::LocalBuildMismatch {
                    stop_command: "shepr server stop",
                    attach_command: Some("shepr"),
                },
            )
        ));
        let notice = local_startup_notice(&error);
        assert!(notice.contains("saved machines stay available"), "{notice}");
        assert!(notice.contains("`shepr server stop --force`"), "{notice}");
    }
}
