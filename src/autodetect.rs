//! Launch flow for `shepr` with no subcommand: attach to the local server,
//! starting it as a detached daemon first when none is listening.

use std::io;
use std::time::Duration;

/// Maximum time to wait for a freshly spawned server's client socket.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Checks the local server, starts it when needed, then runs the client.
///
/// A running server of a different build fails the launch with stop guidance
/// for the resolved session and socket target. With saved machines
/// configured, local startup failures only warn so the remote machines stay
/// reachable; the Local endpoint's handshake then reports the problem.
pub fn auto_detect_launch(
    saved_federation: bool,
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
    run_client: impl FnOnce(&shepr_config::ValidatedConfig, &shepr_config::AppPaths) -> io::Result<()>,
) -> io::Result<()> {
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
    // enabled. With saved machines a mismatch only downgrades to a warning
    // below, so they stay reachable; the Local endpoint's own handshake then
    // rejects the different build with the build-identity preamble error.
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
        tracing::warn!(%error, "Local startup failed; keeping saved machines available");
    }

    run_client(config, paths)
}
