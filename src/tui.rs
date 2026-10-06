//! Launch flow for `shepr` with no subcommand: check the terminal, run the
//! startup preflight, then attach to the local server, starting it as a
//! detached daemon first when none is listening.

use std::io;
use std::time::Duration;

use crate::cli::{self, CliError, CliResult};
use crate::{ProcessExit, init_client_logging, preflight};

/// Runs the TUI launch for a resolved client configuration.
pub(crate) fn launch(
    loaded_config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
) -> CliResult<ProcessExit> {
    ensure_terminal_geometry().map_err(CliError::Terminal)?;

    init_client_logging(paths)?;
    // Prompts and restart offers must run before the client takes the
    // terminal: it connects to machines with BatchMode and cannot answer one.
    let connectors = preflight::run(loaded_config, paths);
    let client = auto_detect_launch(
        loaded_config,
        paths,
        shepr_launch::local_server::SERVER_READY_TIMEOUT,
        connectors,
        shepr_client::run_client_with_connectors,
    )
    .map_err(CliError::Launch)?;
    cli::finish_client(
        client,
        paths.server_address(),
        !loaded_config.machines().is_empty(),
    )
    .map(ProcessExit::from_cli_code)
}

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
/// the TUI exits, and the local endpoint's handshake reports a build mismatch
/// with the same guidance as its status in the sidebar.
///
/// A launch failure before the client runs is the error; once the client has
/// run, its own result is handed back untouched for the caller to report.
fn auto_detect_launch<T>(
    config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
    server_ready_timeout: Duration,
    connectors: Vec<shepr_remote::MachineSshConnector>,
    run_client: impl FnOnce(
        &shepr_config::ValidatedClientConfig,
        &shepr_paths::AppPaths,
        Vec<shepr_remote::MachineSshConnector>,
    ) -> T,
) -> Result<T, shepr_launch::local_server::LaunchError> {
    let socket_path = paths.server_address().socket().to_path_buf();
    tracing::info!(path = %socket_path.display(), "auto-detect launch starting");

    // The running server is checked whether or not machines are
    // configured. With configured machines a mismatch does not end the launch below,
    // so they stay reachable; the local endpoint's own handshake then rejects
    // the different build and shows the same guidance.
    let startup = shepr_launch::local_server::ensure_running(
        paths,
        server_ready_timeout,
        shepr_launch::local_server::BuildCheck::BeforeAttach,
    );
    if let Err(error) = startup {
        if config.machines().is_empty() {
            return Err(error);
        }
        // Keep the full refusal visible even though the client will remain open
        // for the configured machines; the endpoint state omits this startup detail.
        crate::cli::print_notice(&local_startup_notice(&error));
    }

    Ok(run_client(config, paths, connectors))
}

/// Rejects a TUI launch with no usable terminal geometry before preflight can
/// authenticate machines or offer to restart a server.
fn ensure_terminal_geometry() -> io::Result<()> {
    shepr_platform::terminal_grid_size()
        .map(|_| ())
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                shepr_launch::guidance::terminal_geometry_failure(&err),
            )
        })
}

/// What the operator is told when the local server fails to start or is
/// refused while configured machines keep the client running.
fn local_startup_notice(error: &shepr_launch::local_server::LaunchError) -> String {
    shepr_launch::guidance::local_startup_notice(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user-facing notice preserves every line of the startup refusal.
    #[test]
    fn the_local_startup_notice_preserves_the_full_refusal() {
        let error = shepr_launch::local_server::LaunchError::DifferentBuild {
            message: concat!(
                "the running server refused this build. ",
                "Stopping it also exits its pane processes; run `shepr stop`, then `shepr`."
            )
            .to_owned(),
        };
        let notice = local_startup_notice(&error);
        assert_eq!(
            notice,
            concat!(
                "shepr: the local server is unavailable; configured machines stay available.\n",
                "the running server refused this build. Stopping it also exits its pane processes; ",
                "run `shepr stop`, then `shepr`."
            )
        );
        assert!(!notice.contains("--force"), "{notice}");
    }
}
