//! Launch flow for `shepr` with no subcommand: attach to the local server,
//! starting it as a detached daemon first when none is listening.

use std::io;
use std::time::Duration;

/// Maximum time to wait for a freshly spawned server's client socket.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Checks the local server, starts it when needed, then runs the client.
///
/// A running server of a different build fails the launch with guidance for
/// the resolved session and socket target. With saved machines configured, a
/// local startup failure does not end the launch, so the remote machines stay
/// reachable. It is still refused, not swallowed: the failure is printed to
/// stderr before the TUI takes the terminal, where it is on screen again once
/// the TUI exits, and the Local endpoint's handshake reports a build mismatch
/// with the same session-aware guidance as its status in the sidebar.
///
/// A launch failure before the client runs is the error; once the client has
/// run, its own result is handed back untouched for the caller to report.
pub(crate) fn auto_detect_launch<T>(
    saved_federation: bool,
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    run_client: impl FnOnce(&shepr_config::ValidatedConfig, &shepr_config::AppPaths) -> T,
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
    let startup = match shepr_remote::local_server::is_server_listening(paths) {
        Ok(true) => {
            tracing::info!("server already running, attaching as client");
            shepr_remote::local_server::validate_running_server_compatibility(paths)
        }
        Ok(false) => {
            tracing::info!("no server running, spawning server daemon");
            shepr_remote::local_server::spawn_server_daemon(paths).and_then(|_| {
                shepr_remote::local_server::wait_for_server_socket(
                    &socket_path,
                    SERVER_READY_TIMEOUT,
                    paths,
                )
            })
        }
        Err(error) => Err(error),
    };
    if let Err(error) = startup {
        if !saved_federation {
            return Err(error);
        }
        // No tracing subscriber is installed in this process yet, so a log
        // line here would reach no one.
        crate::cli::print_notice(&local_startup_notice(&error));
    }

    Ok(run_client(config, paths))
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
            shepr_api::session::restart_after_update_guidance("shepr server stop", Some("shepr"))
        ));
        let notice = local_startup_notice(&error);
        assert!(notice.contains("saved machines stay available"), "{notice}");
        assert!(notice.contains("--session <name>"), "{notice}");
        assert!(notice.contains("`shepr server stop --force`"), "{notice}");
    }
}
