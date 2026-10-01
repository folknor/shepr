//! Remote-host side of the SSH stdio bridge.

use std::io;

/// Relays this process's stdio to the server socket until either side
/// closes or the idle watchdog fires. The outcome goes back to the binary: on
/// [`shepr_platform::RemoteBridgeOutcome::IdleExpired`] it must end the
/// process promptly with status 1, without writing to stdout.
pub fn run_remote_client_bridge(
    paths: &shepr_config::AppPaths,
) -> io::Result<shepr_platform::RemoteBridgeOutcome> {
    let status = ensure_remote_server_running(paths)?;
    // A server of another build is answered here, never connected to: its
    // socket may not speak this build's client protocol at all, and the
    // client must still read a typed mismatch rather than an EOF it would
    // retry forever.
    if !shepr_protocol::is_this_build(&status.build_id) {
        shepr_platform::answer_remote_bridge(&shepr_protocol::preamble::preamble_for(
            &status.build_id,
        ))?;
        return Ok(shepr_platform::RemoteBridgeOutcome::Closed);
    }

    let socket_path = paths.server_address().socket().to_path_buf();
    // The server's owner is checked before the first relayed byte reaches it.
    let stream =
        shepr_platform::ipc::connect_trusted_local_stream(&socket_path).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "failed to connect to remote Shepr server socket {}: {err}",
                    socket_path.display()
                ),
            )
        })?;

    shepr_platform::forward_remote_bridge_stdio(stream, true)
}

/// Starts the server when none is listening, through the launcher the local
/// TUI uses: a `shepr-server` beside this executable, started under the launch
/// lock and verified to be this build before the bridge relays anything. A
/// running server of another build is returned, not refused: the bridge then
/// answers the client with that build's preamble, so the client reports a
/// typed mismatch, which it classifies as needing attention, whatever socket
/// layout that server has. Failing here instead would reach the client only as
/// this command's stderr and exit status, which it classifies as an ordinary
/// retryable failure. Launch failures reach the client that way, as stderr
/// text: the launcher's messages carry the daemon's boot output.
fn ensure_remote_server_running(
    paths: &shepr_config::AppPaths,
) -> io::Result<shepr_api::RuntimeStatus> {
    super::local_server::ensure_running(
        paths,
        super::local_server::SERVER_READY_TIMEOUT,
        super::local_server::BuildCheck::AtClientHandshake,
    )
}
