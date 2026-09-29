//! Remote-host side of the SSH stdio bridge.

use std::io;

/// Relays this process's stdio to the server's client socket until either side
/// closes or the idle watchdog fires. The outcome goes back to the binary: on
/// [`shepr_platform::RemoteBridgeOutcome::IdleExpired`] it must end the
/// process promptly with status 1, without writing to stdout.
pub fn run_remote_client_bridge(
    paths: &shepr_config::AppPaths,
) -> io::Result<shepr_platform::RemoteBridgeOutcome> {
    ensure_remote_server_running(paths)?;

    let socket_path = paths.server_address().client_socket().to_path_buf();
    // The server's owner is checked before the first relayed byte reaches it.
    let stream =
        shepr_platform::ipc::connect_trusted_local_stream(&socket_path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "failed to connect to remote Shepr client socket {}: {err}",
                    socket_path.display()
                ),
            )
        })?;

    shepr_platform::forward_remote_bridge_stdio(stream, true)
}

/// Starts the server when none is listening, through the launcher the local
/// TUI uses: a `shepr-server` beside this executable, started under the launch
/// lock and verified to be this build before the bridge relays anything. A
/// running server of another build is not screened here: the client's
/// handshake through this bridge reads its build-identity preamble and reports
/// a typed mismatch, which the client classifies as needing attention. Failing
/// here instead would reach the client only as this command's stderr and exit
/// status, which it classifies as an ordinary retryable failure. Launch
/// failures reach the client the same way, as stderr text: the launcher's
/// messages carry the daemon's boot output.
fn ensure_remote_server_running(paths: &shepr_config::AppPaths) -> io::Result<()> {
    super::local_server::ensure_running(
        paths,
        super::local_server::SERVER_READY_TIMEOUT,
        super::local_server::BuildCheck::AtClientHandshake,
    )
}
